//! Applying resume data to a recheck.
//!
//! This ties the pure restore plan to a [`Rechecker`]: load a document, observe
//! the files, run the planned `Only` selection, and fall back to a full
//! recheck when a spot check fails. A missing or unusable document is not an
//! error; it just means every readable piece is rechecked.

use std::sync::Arc;

use thiserror::Error;
use tokio::sync::mpsc;

use super::{
    LoadError, RestoreError, RestorePlan, ResumeDocument, ResumeStore, ValidationError,
    plan_restore,
};
use crate::bitfield::Bitfield;
use crate::metainfo::InfoHash;
use crate::piece::{RecheckCancel, RecheckError, RecheckProgress, RecheckSelection, Rechecker};
use crate::storage::ObservedFile;
use crate::storage::layout::FileMap;
use crate::storage::thread::StorageHandleError;

/// Why a restore used the bits it did.
///
/// The session logs this source when a torrent starts checking. A resume
/// source means verified bits were kept; any other source means every readable
/// piece was rechecked.
#[derive(Debug)]
pub enum RestoreSource {
    /// A valid document supplied the verified bits.
    Resume,
    /// No document existed, so every readable piece was rechecked.
    NoResume,
    /// The document could not be read or decoded, so it was ignored.
    LoadFailed(LoadError),
    /// The document did not describe this torrent, so it was ignored.
    Invalid(ValidationError),
    /// A sampled verified piece failed, so every readable piece was rechecked.
    SpotCheckFailed,
}

/// Final bitfield and the reason it was produced.
#[derive(Debug)]
pub struct RestoreOutcome {
    bits: Bitfield,
    source: RestoreSource,
    checked: u32,
}

impl RestoreOutcome {
    /// Bitfield after every recheck finished.
    pub fn bits(&self) -> &Bitfield {
        &self.bits
    }

    /// Consumes the outcome and returns its final bitfield.
    pub fn into_bits(self) -> Bitfield {
        self.bits
    }

    /// Why the restore used these bits.
    pub fn source(&self) -> &RestoreSource {
        &self.source
    }

    /// Number of pieces read across every recheck pass.
    ///
    /// A spot-check failure runs a second, full pass, so this can exceed the
    /// torrent's piece count.
    pub fn checked(&self) -> u32 {
        self.checked
    }
}

/// Errors that stop a restore.
///
/// A missing, corrupt, or invalid document is handled by falling back to a
/// full recheck and is reported through [`RestoreSource`], not here.
#[derive(Debug, Error)]
pub enum RestoreRunError {
    /// File metadata could not be observed.
    #[error("could not observe files for resume: {0}")]
    Observe(#[from] StorageHandleError),
    /// The observed files and the torrent did not line up.
    #[error("could not plan resume restore: {0}")]
    Plan(#[from] RestoreError),
    /// The blocking planning task was cancelled or panicked.
    #[error("resume restore planning task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    /// The recheck stopped with an error or was cancelled.
    #[error(transparent)]
    Recheck(#[from] RecheckError),
}

/// Plans a restore on the blocking pool.
///
/// Validation and sampling scan the whole bitfield, so they must not run on a
/// runtime worker. A document that does not describe the torrent is not an
/// error: the same job plans again without it, and the validation error is
/// returned so the caller can report the fallback.
async fn plan_with_fallback(
    map: Arc<FileMap>,
    info_hash: InfoHash,
    document: Option<ResumeDocument>,
    observed: Vec<ObservedFile>,
) -> Result<(Option<ValidationError>, RestorePlan), RestoreRunError> {
    let planned = tokio::task::spawn_blocking(move || {
        match plan_restore(&map, &info_hash, document.as_ref(), &observed) {
            Ok(plan) => Ok((None, plan)),
            Err(RestoreError::Invalid(error)) => {
                let plan = plan_restore(&map, &info_hash, None, &observed)?;
                Ok((Some(error), plan))
            }
            Err(error) => Err(error),
        }
    })
    .await
    .map_err(RestoreRunError::Task)?;

    planned.map_err(RestoreRunError::Plan)
}

impl ResumeStore {
    /// Restores verified bits and rechecks what cannot be trusted.
    ///
    /// A valid document clears pieces covered by missing, short, or changed
    /// files and samples up to [`SPOT_CHECK_PIECES`](super::SPOT_CHECK_PIECES)
    /// verified pieces. Only those pieces are read. If a sampled piece fails,
    /// a full recheck runs because unchanged metadata no longer proves the
    /// remaining bits are intact.
    ///
    /// Without a usable document, every readable piece is rechecked; pieces
    /// covered by a missing or short file are cleared without a read. A
    /// missing, corrupt, or mismatched document never fails the torrent.
    ///
    /// The caller must keep the torrent out of the download scheduler for the
    /// duration, as with any [`Rechecker::run`].
    pub async fn restore(
        &self,
        rechecker: &Rechecker,
        cancel: &RecheckCancel,
        progress: Option<&mpsc::Sender<RecheckProgress>>,
    ) -> Result<RestoreOutcome, RestoreRunError> {
        let info_hash = rechecker.info_hash();
        let (document, mut source) = match self.load(&info_hash).await {
            Ok(Some(document)) => (Some(document), RestoreSource::Resume),
            Ok(None) => (None, RestoreSource::NoResume),
            Err(error) => (None, RestoreSource::LoadFailed(error)),
        };

        let observed = rechecker.storage().observe_files().await?;
        let (invalid, plan) =
            plan_with_fallback(rechecker.shared_map(), info_hash, document, observed).await?;
        if let Some(error) = invalid {
            source = RestoreSource::Invalid(error);
        }

        let first = rechecker
            .run(
                RecheckSelection::Only(plan.recheck()),
                plan.bits(),
                cancel,
                progress,
            )
            .await?;
        let mut checked = first.checked();

        let bits = if plan
            .spot_checks()
            .iter()
            .any(|&piece| !matches!(first.bits().get(piece), Ok(true)))
        {
            source = RestoreSource::SpotCheckFailed;
            let full = rechecker
                .run(RecheckSelection::All, first.bits(), cancel, progress)
                .await?;
            checked = checked.saturating_add(full.checked());
            full.into_bits()
        } else {
            first.into_bits()
        };

        Ok(RestoreOutcome {
            bits,
            source,
            checked,
        })
    }
}

#[cfg(test)]
mod tests;
