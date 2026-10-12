//! Windows: embed the Fp app icon into `fakeprint.exe`.

fn main() {
    let icon = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/assets/fakeprint.ico"
    );
    println!("cargo:rerun-if-changed={icon}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon);
    res.set("ProductName", "fakeprint")
        .set("FileDescription", "Fake WSD printer for testing")
        .set(
            "LegalCopyright",
            "Copyright (c) the fakeprint contributors. MIT OR Apache-2.0.",
        )
        .set("OriginalFilename", "fakeprint.exe")
        .set("InternalName", "fakeprint");
    if let Err(e) = res.compile() {
        println!("cargo:warning=fakeprint.exe built without icon/resources: {e}");
    }
}
