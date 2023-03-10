//! Core functionality for the Pylon application.
//!
//! This library derives from and wraps over the [`magic-wormhole`] library to provide custom types and functionality.
//!
//! [`magic-wormhole`]: https://crates.io/crates/magic-wormhole

pub mod consts;

use std::borrow::Cow;
use std::error::Error;
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;

use derive_builder::Builder;
use magic_wormhole::rendezvous::DEFAULT_RENDEZVOUS_SERVER;
use magic_wormhole::transfer::{self, AppVersion, ReceiveRequest, TransferError};
use magic_wormhole::transit::{
    self, RelayHint, RelayHintParseError, TransitInfo, DEFAULT_RELAY_SERVER,
};
use magic_wormhole::{AppConfig, AppID, Code, Wormhole, WormholeError};
use serde::Serialize;
use smol::fs::File;
use thiserror::Error;
use url::ParseError;

/// Awaitable object that will perform the client-client handshake and yield the wormhole object on success.
type Handshake = dyn Future<Output = Result<Wormhole, WormholeError>> + Unpin + Send + Sync;

/// Type alias for magic-wormhole transit abilities.
pub type Abilities = transit::Abilities;

/// Custom error type for the various errors a Pylon may encounter.
///
/// These could be errors generated by the underlying wormhole library (some of which we handle explicitly and some of
/// which we don't), or custom validation/other errors that we may want to return.
#[derive(Debug, Error)]
pub enum PylonError {
    /// Wormhole code generation failed for some reason.
    /// Possibly because the underlying wormhole has already been initialized.
    #[error("Error generating wormhole code: {0}")]
    CodegenError(Box<str>),
    /// The provided relay server URL could not be parsed.
    /// This is just a wrapper over the underlying wormhole library's error of the same name.
    #[error("Error parsing relay server URL")]
    RelayHintParseError(
        #[from]
        #[source]
        RelayHintParseError,
    ),
    /// Error parsing a URL. Eg: rendezvous server URL or relay server URL.
    /// This is just a wrapper over the `url` library's `ParseError`.
    #[error(transparent)]
    UrlParseError(#[from] ParseError),
    /// Error occured during the transfer.
    /// This is just a wrapper over the underlying womhole library's error of the same name.
    #[error("Error occured during transfer")]
    TransferError(
        #[from]
        #[source]
        TransferError,
    ),
    /// An error occured with the underlying wormhole library that we aren't explicitly matching against.
    #[error(transparent)]
    InternalError(#[from] WormholeError),
    #[error(transparent)]
    /// An error occured with building the Pylon.
    /// This is just a wrapper to allow easy propagation of builder errors with the `?` operator.
    BuilderError(#[from] PylonBuilderError),
    /// Generic error messages.
    #[error("An error occured: {0}")]
    Error(
        #[from]
        #[source]
        Box<dyn Error>,
    ),
}

impl Serialize for PylonError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.to_string().as_ref())
    }
}

// TODO: improve documentation
/// High-level wrapper over a magic-wormhole that allows for secure file-transfers.
#[derive(Serialize, Builder)]
#[serde(rename_all = "camelCase")]
pub struct Pylon {
    id: String,
    #[builder(default = "DEFAULT_RELAY_SERVER.into()")]
    relay_url: String,
    #[builder(default = "DEFAULT_RENDEZVOUS_SERVER.into()")]
    rendezvous_url: String,
    #[builder(default = "Abilities::ALL_ABILITIES")]
    abilities: Abilities,
    #[serde(skip)]
    #[builder(setter(skip))]
    handshake: Option<Box<Handshake>>,
    #[serde(skip)]
    #[builder(setter(skip))]
    transfer_request: Option<ReceiveRequest>,
}

impl Pylon {
    /// Builds and returns a wormhole app config.
    fn config(&self) -> AppConfig<AppVersion> {
        AppConfig {
            id: AppID(Cow::from(self.id.clone())),
            rendezvous_url: Cow::from(self.rendezvous_url.clone()),
            app_version: AppVersion {},
        }
    }

    // TODO: add example(s)
    /// Returns a generated wormhole code and connects to the rendezvous server.
    ///
    /// # Arguments
    ///
    /// * `code_length` - The required length of the wormhole code.
    pub async fn gen_code(&mut self, code_length: usize) -> Result<String, PylonError> {
        if let Some(_) = &self.handshake {
            return Err(PylonError::CodegenError(
                "The current Pylon already has a pending handshake".into(),
            ));
        }

        let (welcome, handshake) =
            Wormhole::connect_without_code(self.config(), code_length).await?;
        self.handshake = Some(Box::new(Box::pin(handshake)));

        Ok(welcome.code.0)
    }

    // TODO: add example(s)
    /// Sends a file over the wormhole network to the receiver Pylon.
    ///
    /// # Arguments
    ///
    /// * `file` - The path of the file to send.
    /// * `progress_handler` - Callback function that accepts the number of bytes sent and the total number of bytes to send.
    /// * `cancel_handler` - Callback function to request cancellation of the file transfer.
    pub async fn send_file<F, P, C>(
        &mut self,
        file: F,
        progress_handler: P,
        cancel_handler: C,
    ) -> Result<(), PylonError>
    where
        F: AsRef<Path>,
        P: FnMut(u64, u64) + 'static,
        C: Future<Output = ()>,
    {
        let file_name = file
            .as_ref()
            .file_name()
            .ok_or(PylonError::Error("could not extract file name".into()))?
            .to_str()
            .ok_or(PylonError::Error(
                "could not convert file name to str".into(),
            ))?;
        let mut file = File::open(&file)
            .await
            .map_err(|e| PylonError::Error(e.into()))?;
        let file_size = file
            .metadata()
            .await
            .map_err(|e| PylonError::Error(e.into()))?
            .len();
        // TODO: allow caller to specify transit handler, abilities and relay hints
        let transit_handler = |_: TransitInfo, _: SocketAddr| {};
        let transit_abilities = self.abilities;
        let relay_hints = vec![RelayHint::from_urls(None, [self.relay_url.parse()?])?];

        let sender = match self.handshake.take() {
            None => {
                return Err(PylonError::Error(
                    "There is currently no active handshake".into(),
                ))
            }
            Some(h) => {
                let wh = h.await?;
                transfer::send_file(
                    wh,
                    relay_hints,
                    &mut file,
                    file_name,
                    file_size,
                    transit_abilities,
                    transit_handler,
                    progress_handler,
                    cancel_handler,
                )
            }
        };
        sender.await?;

        Ok(())
    }

    // TODO: add example(s)
    /// Initiates a request for a file transfer from the sender Pylon.
    ///
    /// # Arguments
    ///
    /// * `code` - The wormhole code to authenticate the connection.
    /// * `cancel_handler` - Callback function to request cancellation of the file transfer.
    pub async fn request_file<C: Future<Output = ()>>(
        &mut self,
        code: String,
        cancel_handler: C,
    ) -> Result<(), PylonError> {
        // TODO: allow caller to specify transit abilities and relay hints
        let transit_abilities = self.abilities;
        let relay_hints = vec![RelayHint::from_urls(None, [self.relay_url.parse()?])?];

        let (_, wh) = Wormhole::connect_with_code(self.config(), Code(code)).await?;
        let request =
            transfer::request_file(wh, relay_hints, transit_abilities, cancel_handler).await?;
        self.transfer_request = request;

        Ok(())
    }

    // TODO: add example(s)
    /// Accepts a file transfer and receives a file over the wormhole network from the sender Pylon.
    ///
    /// # Arguments
    ///
    /// * `file` - The destination file path.
    /// * `progress_handler` - Callback function that accepts the number of bytes received and the total number of bytes
    ///                        to receive.
    /// * `cancel_handler` - Callback function to request cancellation of the file transfer.
    pub async fn receive_file<F, P, C>(
        &mut self,
        file: F,
        progress_handler: P,
        cancel_handler: C,
    ) -> Result<(), PylonError>
    where
        F: AsRef<Path>,
        P: FnMut(u64, u64) + 'static,
        C: Future<Output = ()>,
    {
        let mut file = File::create(&file)
            .await
            .map_err(|e| PylonError::Error(e.into()))?;
        // TODO: allow caller to specify transit abilities
        let transit_handler = |_: TransitInfo, _: SocketAddr| {};
        match self.transfer_request.take() {
            Some(r) => {
                // TODO: allow caller to accept or reject transfer
                r.accept(transit_handler, progress_handler, &mut file, cancel_handler)
                    .await?;
            }
            None => {
                return Err(PylonError::Error(
                    "There is currently no active transfer request".into(),
                ));
            }
        }

        Ok(())
    }

    /// Destroys the Pylon.
    ///
    /// Currently, we just drop the Pylon. A cleaner shutdown process MAY be implemented in the future, but that depends
    /// on progress in the underlying [`magic-wormhole`] library's clean shutdown implementation.
    ///
    /// [`magic-wormhole`]: https://crates.io/crates/magic-wormhole
    pub fn destroy(self) {
        drop(self);
    }
}
