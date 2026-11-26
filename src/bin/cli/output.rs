use sendmer::Format;

pub(super) fn print_hash(hash: &iroh_blobs::Hash, format: Format) -> String {
    match format {
        Format::Hex => hash.to_hex(),
        Format::Cid => hash.to_string(),
    }
}
