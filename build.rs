fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let ico_path = format!("{}/icon.ico", out_dir);

    generate_icon(&ico_path);

    let mut res = winresource::WindowsResource::new();
    res.set_icon(&ico_path);
    res.set_manifest(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*"
      />
    </dependentAssembly>
  </dependency>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>"#);
    if let Err(e) = res.compile() {
        eprintln!("cargo:warning=Failed to embed icon: {}", e);
    }
}

fn generate_icon(path: &str) {
    let size: u32 = 32;

    // Colors (BGRA)
    let bg = [0xD2u8, 0x76, 0x19, 0xFF]; // #1976D2 (Material Blue)
    let fg = [0xFF, 0xFF, 0xFF, 0xFF]; // White

    // Build pixel data bottom-to-top (BMP format)
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for bmp_row in 0..size {
        let y = size - 1 - bmp_row;
        for x in 0..size {
            let color = if is_letter_t(x, y) { &fg } else { &bg };
            pixels.extend_from_slice(color);
        }
    }

    // AND mask (all zeros = fully opaque)
    let mask_row_size = ((size + 31) / 32) * 4;
    let mask = vec![0u8; (mask_row_size * size) as usize];

    let image_data_size: u32 = 40 + (size * size * 4) + (mask_row_size * size);

    let mut data = Vec::new();

    // ICO header
    data.extend_from_slice(&0u16.to_le_bytes()); // reserved
    data.extend_from_slice(&1u16.to_le_bytes()); // type = icon
    data.extend_from_slice(&1u16.to_le_bytes()); // count = 1

    // Directory entry
    data.push(size as u8);
    data.push(size as u8);
    data.push(0); // no color palette
    data.push(0); // reserved
    data.extend_from_slice(&1u16.to_le_bytes()); // color planes
    data.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    data.extend_from_slice(&image_data_size.to_le_bytes());
    data.extend_from_slice(&22u32.to_le_bytes()); // offset = 6 + 16

    // BITMAPINFOHEADER
    data.extend_from_slice(&40u32.to_le_bytes()); // header size
    data.extend_from_slice(&(size as i32).to_le_bytes()); // width
    data.extend_from_slice(&((size * 2) as i32).to_le_bytes()); // height (doubled for ICO)
    data.extend_from_slice(&1u16.to_le_bytes()); // planes
    data.extend_from_slice(&32u16.to_le_bytes()); // bpp
    data.extend_from_slice(&0u32.to_le_bytes()); // compression
    data.extend_from_slice(&0u32.to_le_bytes()); // image size
    data.extend_from_slice(&0u32.to_le_bytes()); // x pixels/meter
    data.extend_from_slice(&0u32.to_le_bytes()); // y pixels/meter
    data.extend_from_slice(&0u32.to_le_bytes()); // colors used
    data.extend_from_slice(&0u32.to_le_bytes()); // important colors

    // Pixel data + mask
    data.extend_from_slice(&pixels);
    data.extend_from_slice(&mask);

    std::fs::write(path, data).unwrap();
}

fn is_letter_t(x: u32, y: u32) -> bool {
    // "T" letter in a 32x32 grid with 3px margin
    let top_bar = y >= 5 && y <= 9 && x >= 6 && x <= 25;
    let stem = y >= 5 && y <= 26 && x >= 13 && x <= 18;
    top_bar || stem
}
