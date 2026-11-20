//! Baseline against a factory-signed `xclbin2` image from the AMD Windows driver
//! package. The parser is checked against real vendor output, not just synthetic
//! fixtures. Skipped when no image is available.

use data_binary_xclbin::{SectionKind, Xclbin, XclbinBuilder};

fn vendor_image() -> Option<Vec<u8>> {
    let candidate = std::env::var("BOOST_XCLBIN_PATH").ok().or_else(|| {
        let glob = std::path::Path::new(r"C:\Windows\System32\DriverStore\FileRepository");
        std::fs::read_dir(glob).ok()?.flatten().find_map(|dir| {
            let file = dir.path().join("validate_1502_00.xclbin");
            file.is_file().then_some(file.to_string_lossy().into_owned())
        })
    })?;
    std::fs::read(&candidate).ok()
}

#[test]
fn parses_a_factory_signed_image() {
    let Some(bytes) = vendor_image()
    else {
        eprintln!("SKIP: no vendor xclbin (set BOOST_XCLBIN_PATH)");
        return;
    };
    let image = Xclbin::parse(&bytes).expect("vendor image must parse");
    assert_eq!(image.length, bytes.len() as u64);
    assert_eq!(image.signature_length, -1, "driver-package images are unsigned");
    assert!(!image.platform.is_empty());
    assert!(!image.sections.is_empty());

    for section in &image.sections {
        assert_eq!(section.offset % 8, 0, "section `{}` unaligned", section.name);
        assert!((section.offset + section.size) as usize <= bytes.len(), "section `{}` beyond image", section.name);
    }

    let metadata = image.section(SectionKind::EmbeddedMetadata).expect("embedded metadata");
    assert!(metadata.data.starts_with(b"<?xml"), "metadata payload must be XML");
}

#[test]
fn repackages_vendor_sections_into_a_self_authored_container() {
    let Some(bytes) = vendor_image()
    else {
        eprintln!("SKIP: no vendor xclbin (set BOOST_XCLBIN_PATH)");
        return;
    };
    let original = Xclbin::parse(&bytes).expect("vendor image must parse");

    // Container authoring gate: move every vendor payload into a self-written
    // `xclbin2` and require byte-identical section contents after re-parse.
    // This signs off on container structure only, never on device code.
    let mut builder = XclbinBuilder::new().platform(original.platform).uuid(original.uuid).time_stamp(original.time_stamp);
    for section in &original.sections {
        builder = builder.add_raw_section(section.kind, section.name, section.data.to_vec());
    }
    let authored = builder.build();

    let repacked = Xclbin::parse(&authored).expect("authored container must parse");
    assert_eq!(repacked.sections.len(), original.sections.len());
    assert_eq!(repacked.platform, original.platform);
    assert_eq!(repacked.uuid, original.uuid);
    for (mine, theirs) in repacked.sections.iter().zip(&original.sections) {
        assert_eq!(mine.kind, theirs.kind);
        assert_eq!(mine.data, theirs.data, "section `{}` payload changed", theirs.name);
    }
}
