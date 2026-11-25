//! Native project container.
//!
//! One `open-ppt/deck` project is a single ZIP archive: `deck.json` holds the
//! canonical authored document (without asset bytes), and every asset payload
//! lives at `assets/<assetId>`. Both directions are pure byte transformations;
//! the caller owns file paths, temporary files and atomic renames.
//!
//! A project never carries a second JSON shape and never stores asset paths as
//! the only copy of the bytes, so reopening cannot depend on the author's disk.

use boost_source::binary::MemorySource;
use data_binary_zip::{ZipEntry, ZipWriter, index_zip_bytes, read_stored_payload};

use super::{
    edit::sha256_hex,
    error::{DeckError, codes},
    model::{Asset, Deck, DeckDocument},
};

/// Canonical document entry inside the archive.
pub const DECK_ENTRY: &str = "deck.json";

/// Serialises a deck into a native project archive.
pub fn encode_project(deck: &Deck) -> Result<Vec<u8>, DeckError> {
    let document = deck.document();
    let mut assets: Vec<&Asset> = document.assets.iter().collect();
    assets.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
    for asset in &assets {
        if asset.bytes.is_empty() {
            return Err(DeckError::asset_missing(asset.id.as_str()));
        }
        if sha256_hex(&asset.bytes) != asset.sha256 {
            return Err(DeckError::new(
                codes::FORMAT_INVALID_PACKAGE,
                format!("asset `{}` bytes no longer match their recorded hash", asset.id),
            ));
        }
    }

    let json = serde_json::to_vec(document)
        .map_err(|error| DeckError::new(codes::FORMAT_INVALID_PACKAGE, format!("could not encode deck.json: {error}")))?;
    let mut writer = ZipWriter::new();
    writer.push(ZipEntry::stored(DECK_ENTRY, json));
    for asset in assets {
        writer.push(ZipEntry::stored(format!("assets/{}", asset.id), asset.bytes.clone()));
    }
    writer.into_bytes().map_err(|error| DeckError::new(codes::FORMAT_INVALID_PACKAGE, format!("could not write project archive: {error}")))
}

/// Reads a native project archive back into a deck at its committed revision.
pub fn decode_project(bytes: &[u8]) -> Result<Deck, DeckError> {
    let index = index_zip_bytes("deck.project", bytes.to_vec())
        .map_err(|error| DeckError::new(codes::FORMAT_INVALID_PACKAGE, format!("not a readable project archive: {error}")))?;
    let mut deck_members = index.members.iter().filter(|member| member.raw_name == DECK_ENTRY.as_bytes());
    let member = deck_members.next().ok_or_else(|| DeckError::new(codes::FORMAT_INVALID_PACKAGE, "the archive has no deck.json"))?;
    if deck_members.next().is_some() {
        return Err(DeckError::new(codes::FORMAT_INVALID_PACKAGE, "the archive has more than one deck.json"));
    }

    let json = read_member(bytes, member)?;
    let mut document: DeckDocument = serde_json::from_slice(&json)
        .map_err(|error| DeckError::new(codes::FORMAT_INVALID_PACKAGE, format!("deck.json is malformed: {error}")))?;

    for asset in &mut document.assets {
        let name = format!("assets/{}", asset.id);
        let member = index
            .members
            .iter()
            .find(|candidate| candidate.raw_name == name.as_bytes())
            .ok_or_else(|| DeckError::new(codes::FORMAT_INVALID_PACKAGE, format!("asset `{}` has no payload entry", asset.id)))?;
        let payload = read_member(bytes, member)?;
        if sha256_hex(&payload) != asset.sha256 {
            return Err(DeckError::new(
                codes::FORMAT_INVALID_PACKAGE,
                format!("asset `{}` payload does not match its recorded hash", asset.id),
            ));
        }
        asset.bytes = payload;
    }

    Deck::from_document(document)
}

fn read_member(bytes: &[u8], member: &data_binary_zip::ZipMember) -> Result<Vec<u8>, DeckError> {
    let source = MemorySource::from_bytes("deck.project", bytes.to_vec());
    read_stored_payload(source, member)
        .map_err(|error| DeckError::new(codes::FORMAT_INVALID_PACKAGE, format!("project entries must be stored without compression: {error}")))
}
