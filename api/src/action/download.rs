use std::fs::File;
use std::io::{
    self,
    Error as IoError,
    Read,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use reqwest::{Client, Response, StatusCode};
use reqwest::header::Authorization;
use reqwest::header::ContentLength;

use api::url::UrlBuilder;
use crypto::key_set::KeySet;
use crypto::sig::signature_encoded;
use ext::status_code::StatusCodeExt;
use file::remote_file::RemoteFile;
use reader::{EncryptedFileWriter, ProgressReporter, ProgressWriter};
use super::metadata::{
    Error as MetadataError,
    Metadata as MetadataAction,
};

/// A file upload action to a Send server.
pub struct Download<'a> {
    /// The remote file to download.
    file: &'a RemoteFile,

    /// The target file or directory, to download the file to.
    target: PathBuf,

    /// An optional password to decrypt a protected file.
    password: Option<String>,

    /// Check whether the file exists (recommended).
    check_exists: bool,
}

impl<'a> Download<'a> {
    /// Construct a new download action for the given remote file.
    /// It is recommended to check whether the file exists,
    /// unless that is already done.
    pub fn new(
        file: &'a RemoteFile,
        target: PathBuf,
        password: Option<String>,
        check_exists: bool,
    ) -> Self {
        Self {
            file,
            target,
            password,
            check_exists,
        }
    }

    /// Invoke the download action.
    pub fn invoke(
        self,
        client: &Client,
        reporter: Arc<Mutex<ProgressReporter>>,
    ) -> Result<(), Error> {
        // Create a key set for the file
        let mut key = KeySet::from(self.file, self.password.as_ref());

        // Fetch the file metadata, update the input vector in the key set
        let metadata = MetadataAction::new(
                self.file,
                self.password.clone(),
                self.check_exists,
            )
            .invoke(&client)
            .map_err(|err| match err {
                MetadataError::PasswordRequired => Error::PasswordRequired,
                MetadataError::Expired => Error::Expired,
                _ => err.into(),
            })?;
        key.set_iv(metadata.metadata().iv());

        // Decide what actual file target to use
        let path = self.decide_path(metadata.metadata().name());
        let path_str = path.to_str().unwrap_or("?").to_owned();

        // Open the file we will write to
        // TODO: this should become a temporary file first
        // TODO: use the uploaded file name as default
        let out = File::create(path)
            .map_err(|err| Error::File(
                path_str.clone(),
                FileError::Create(err),
            ))?;

        // Create the file reader for downloading
        let (reader, len) = self.create_file_reader(
            &key,
            metadata.nonce().to_vec(),
            &client,
        )?;

        // Create the file writer
        let writer = self.create_file_writer(
            out,
            len,
            &key,
            reporter.clone(),
        ).map_err(|err| Error::File(path_str.clone(), err))?;

        // Download the file
        self.download(reader, writer, len, reporter)?;

        // TODO: return the file path
        // TODO: return the new remote state (does it still exist remote)

        Ok(())
    }

    /// Decide what path we will download the file to.
    ///
    /// A target file or directory, and a file name hint must be given.
    /// The name hint can be derived from the retrieved metadata on this file.
    ///
    /// The name hint is used as file name, if a directory was given.
    fn decide_path(&self, name_hint: &str) -> PathBuf {
        // Return the target if it is an existing file
        if self.target.is_file() {
            return self.target.clone();
        }

        // Append the name hint if this is a directory
        if self.target.is_dir() {
            return self.target.join(name_hint);
        }

        // Return if the parent is an existing directory
        if self.target.parent().map(|p| p.is_dir()).unwrap_or(false) {
            return self.target.clone();
        }

        // TODO: canonicalize the path when possible
        // TODO: allow using `file.toml` as target without directory indication
        // TODO: return a nice error here as the path may be invalid
        // TODO: maybe prompt the user to create the directory
        panic!("Invalid (non-existing) output path given, not yet supported");
    }

    /// Make a download request, and create a reader that downloads the
    /// encrypted file.
    ///
    /// The response representing the file reader is returned along with the
    /// length of the reader content.
    fn create_file_reader(
        &self,
        key: &KeySet,
        meta_nonce: Vec<u8>,
        client: &Client,
    ) -> Result<(Response, u64), DownloadError> {
        // Compute the cryptographic signature
        let sig = signature_encoded(key.auth_key().unwrap(), &meta_nonce)
            .map_err(|_| DownloadError::ComputeSignature)?;

        // Build and send the download request
        let response = client.get(UrlBuilder::api_download(self.file))
            .header(Authorization(
                format!("send-v1 {}", sig)
            ))
            .send()
            .map_err(|_| DownloadError::Request)?;

        // Validate the status code
        let status = response.status();
        if !status.is_success() {
            return Err(DownloadError::RequestStatus(status, status.err_text()));
        }

        // Get the content length
        // TODO: make sure there is enough disk space
        let len = response.headers().get::<ContentLength>()
            .ok_or(DownloadError::NoLength)?.0;

        Ok((response, len))
    }

    /// Create a file writer.
    ///
    /// This writer will will decrypt the input on the fly, and writes the
    /// decrypted data to the given file.
    fn create_file_writer(
        &self,
        file: File,
        len: u64,
        key: &KeySet,
        reporter: Arc<Mutex<ProgressReporter>>,
    ) -> Result<ProgressWriter<EncryptedFileWriter>, FileError> {
        // Build an encrypted writer
        let mut writer = ProgressWriter::new(
            EncryptedFileWriter::new(
                file,
                len as usize,
                KeySet::cipher(),
                key.file_key().unwrap(),
                key.iv(),
            ).map_err(|_| FileError::EncryptedWriter)?
        ).map_err(|_| FileError::EncryptedWriter)?;

        // Set the reporter
        writer.set_reporter(reporter.clone());

        Ok(writer)
    }

    /// Download the file from the reader, and write it to the writer.
    /// The length of the file must also be given.
    /// The status will be reported to the given progress reporter.
    fn download<R: Read>(
        &self,
        mut reader: R,
        mut writer: ProgressWriter<EncryptedFileWriter>,
        len: u64,
        reporter: Arc<Mutex<ProgressReporter>>,
    ) -> Result<(), DownloadError> {
        // Start the writer
        reporter.lock()
            .map_err(|_| DownloadError::Progress)?
            .start(len);

        // Write to the output file
        io::copy(&mut reader, &mut writer).map_err(|_| DownloadError::Download)?;

        // Finish
        reporter.lock()
            .map_err(|_| DownloadError::Progress)?
            .finish();

        // Verify the writer
        if writer.unwrap().verified() {
            Ok(())
        } else {
            Err(DownloadError::Verify)
        }
    }
}

#[derive(Fail, Debug)]
pub enum Error {
    /// An error occurred while fetching the metadata of the file.
    /// This step is required in order to succsessfully decrypt the
    /// file that will be downloaded.
    #[fail(display = "Failed to fetch file metadata")]
    Meta(#[cause] MetadataError),

    /// The given Send file has expired, or did never exist in the first place.
    /// Therefore the file could not be downloaded.
    #[fail(display = "The file has expired or did never exist")]
    Expired,

    /// A password is required, but was not given.
    #[fail(display = "Missing password, password required")]
    PasswordRequired,

    /// An error occurred while downloading the file.
    #[fail(display = "Failed to download the file")]
    Download(#[cause] DownloadError),

    /// An error occurred while decrypting the downloaded file.
    #[fail(display = "Failed to decrypt the downloaded file")]
    Decrypt,

    /// An error occurred while opening or writing to the target file.
    // TODO: show what file this is about
    #[fail(display = "Couldn't use the target file at '{}'", _0)]
    File(String, #[cause] FileError),
}

impl From<MetadataError> for Error {
    fn from(err: MetadataError) -> Error {
        Error::Meta(err)
    }
}

impl From<DownloadError> for Error {
    fn from(err: DownloadError) -> Error {
        Error::Download(err)
    }
}

#[derive(Fail, Debug)]
pub enum DownloadError {
    /// An error occurred while computing the cryptographic signature used for
    /// downloading the file.
    #[fail(display = "Failed to compute cryptographic signature")]
    ComputeSignature,

    /// Sending the request to download the file failed.
    #[fail(display = "Failed to request file download")]
    Request,

    /// The response for downloading the indicated an error and wasn't successful.
    #[fail(display = "Bad HTTP response '{}' while requesting file download", _1)]
    RequestStatus(StatusCode, String),

    /// The length of the file is missing, thus the length of the file to download
    /// couldn't be determined.
    #[fail(display = "Couldn't determine file download length, missing property")]
    NoLength,

    /// Failed to start or update the downloading progress, because of this the
    /// download can't continue.
    #[fail(display = "Failed to update download progress")]
    Progress,

    /// The actual download and decryption process the server.
    /// This covers reading the file from the server, decrypting the file,
    /// and writing it to the file system.
    #[fail(display = "Failed to download the file")]
    Download,

    /// Verifying the downloaded file failed.
    #[fail(display = "File verification failed")]
    Verify,
}

#[derive(Fail, Debug)]
pub enum FileError {
    /// An error occurred while creating or opening the file to write to.
    #[fail(display = "Failed to create or replace the file")]
    Create(#[cause] IoError),

    /// Failed to create an encrypted writer for the file, which is used to
    /// decrypt the downloaded file.
    #[fail(display = "Failed to create file decryptor")]
    EncryptedWriter,
}
