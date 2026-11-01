use downloader::{Download, Downloader};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const WEBGPU_H_COMMIT_ID: &str = "673658bc2bd70ec39fc55ebe6bb0173cf6d0a603";
const WEBGPU_H_SHA256_SUM: &str = "a483031c3fed05ea5dd1c74082a71676c46c5b2b820ccca10da515c033efc997";

fn main() {
    println!("cargo:rerun-if-changed=wgpu.h");

    let webgpu_h_download_dir = download_webgpu_header();

    #[rustfmt::skip]
    let types_to_rename = vec![
        ("WGPUAdapter", "WGPUAdapterImpl"),
        ("WGPUBindGroup", "WGPUBindGroupImpl"),
        ("WGPUBindGroupLayout", "WGPUBindGroupLayoutImpl"),
        ("WGPUBuffer", "WGPUBufferImpl"),
        ("WGPUCommandBuffer", "WGPUCommandBufferImpl"),
        ("WGPUCommandEncoder", "WGPUCommandEncoderImpl"),
        ("WGPUComputePassEncoder", "WGPUComputePassEncoderImpl"),
        ("WGPUComputePipeline", "WGPUComputePipelineImpl"),
        ("WGPUDevice", "WGPUDeviceImpl"),
        ("WGPUInstance", "WGPUInstanceImpl"),
        ("WGPUPipelineLayout", "WGPUPipelineLayoutImpl"),
        ("WGPUQuerySet", "WGPUQuerySetImpl"),
        ("WGPUQueue", "WGPUQueueImpl"),
        ("WGPURenderBundle", "WGPURenderBundleImpl"),
        ("WGPURenderBundleEncoder", "WGPURenderBundleEncoderImpl"),
        ("WGPURenderPassEncoder", "WGPURenderPassEncoderImpl"),
        ("WGPURenderPipeline", "WGPURenderPipelineImpl"),
        ("WGPUSampler", "WGPUSamplerImpl"),
        ("WGPUShaderModule", "WGPUShaderModuleImpl"),
        ("WGPUSurface", "WGPUSurfaceImpl"),
        ("WGPUTexture", "WGPUTextureImpl"),
        ("WGPUTextureView", "WGPUTextureViewImpl"),
    ];

    let mut builder = bindgen::Builder::default()
        .header("wgpu.h")
        .clang_arg(format!("-I{}", webgpu_h_download_dir))
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .allowlist_item("WGPU.*")
        .allowlist_item("wgpu.*")
        .prepend_enum_name(false)
        .size_t_is_usize(true)
        .ignore_functions()
        .layout_tests(true)
        .clang_macro_fallback();

    for (old_name, new_name) in types_to_rename {
        let line = format!("pub type {old_name} = *const crate::{new_name};");
        builder = builder.blocklist_type(old_name).blocklist_type(format!("{old_name}Impl")).raw_line(line);
    }

    let bindings = builder.generate().expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings.write_to_file(out_path.join("bindings.rs")).expect("Couldn't write bindings!");
}

fn download_webgpu_header() -> String {
    println!("cargo:rerun-if-env-changed=WEBGPU_SHIM_HEADER");
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());

    let file = out_path.join("webgpu.h");
    if let Some(header) = env::var_os("WEBGPU_SHIM_HEADER").filter(|header| !header.is_empty()) {
        println!("cargo:rerun-if-changed={}", PathBuf::from(&header).display());
        prepare_webgpu_header(Path::new(&header), &out_path, WEBGPU_H_SHA256_SUM);
    } else if !sha256::try_digest(&file).is_ok_and(|sum| sum == WEBGPU_H_SHA256_SUM) {
        fs::create_dir_all(out_path.clone()).expect("Failed to create output directory");
        let mut downloader = Downloader::builder()
            .download_folder(&out_path)
            .build()
            .expect("Unable to build download builder");
        let downloads = downloader
            .download(&[Download::new(&format!(
                "https://github.com/webgpu-native/webgpu-headers/raw/{WEBGPU_H_COMMIT_ID}/webgpu.h"
            ))])
            .expect("Failed to download maplibre-native static lib")
            .into_iter();
        for download in downloads {
            if let Err(err) = download {
                panic!("Unexpected error from downloader: {err}");
            }
        }
        if let Err(e) = sha256::try_digest(file) {
            panic!("Unable to validate webgpu.h: {e}");
        }
    }

    let out_path = out_path.as_os_str().to_str().expect("Failed to resolve webgpu include dir");
    println!("cargo:rustc-env=WEBGPU_SHIM_WEBGPU_HEADER_INCLUDE_DIR={}", out_path);
    out_path.to_owned()
}

fn prepare_webgpu_header(header: &Path, out_path: &Path, expected_sum: &str) {
    let sum = sha256::try_digest(header).expect("Unable to validate WEBGPU_SHIM_HEADER");
    verify_webgpu_header_checksum(&sum, expected_sum);

    let file = out_path.join("webgpu.h");
    if sha256::try_digest(&file).is_ok_and(|sum| sum == expected_sum) {
        return;
    }

    fs::create_dir_all(out_path).expect("Failed to create output directory");
    // Copies from the Nix store can leave a read-only cache; unlink instead of overwriting it.
    match fs::remove_file(&file) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => panic!("Failed to remove stale webgpu.h: {err}"),
    }
    fs::copy(header, &file).expect("Failed to copy WEBGPU_SHIM_HEADER to webgpu.h");
}

fn verify_webgpu_header_checksum(sum: &str, expected_sum: &str) {
    assert_eq!(sum, expected_sum, "WEBGPU_SHIM_HEADER SHA256 mismatch");
}

#[cfg(test)]
mod tests {
    use super::{WEBGPU_H_SHA256_SUM, prepare_webgpu_header, verify_webgpu_header_checksum};
    use std::fs;
    use std::path::PathBuf;

    fn test_directory(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let directory = std::env::temp_dir().join(format!("webgpu-shim-{name}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn verify_webgpu_header_checksum_accepts_pinned_digest() {
        verify_webgpu_header_checksum(WEBGPU_H_SHA256_SUM, WEBGPU_H_SHA256_SUM);
    }

    #[test]
    #[should_panic(expected = "WEBGPU_SHIM_HEADER SHA256 mismatch")]
    fn verify_webgpu_header_checksum_rejects_mismatched_digest() {
        verify_webgpu_header_checksum("invalid digest", WEBGPU_H_SHA256_SUM);
    }

    #[cfg(unix)]
    #[test]
    fn prepare_webgpu_header_reuses_read_only_copy_and_replaces_corruption() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = test_directory("read-only");
        let header = directory.join("override.h");
        let contents = b"test WebGPU header";
        let expected_sum = sha256::digest(contents);
        fs::write(&header, contents).unwrap();
        fs::set_permissions(&header, fs::Permissions::from_mode(0o444)).unwrap();
        let out_path = directory.join("nested/out");
        let file = out_path.join("webgpu.h");

        prepare_webgpu_header(&header, &out_path, &expected_sum);
        let first = fs::metadata(&file).unwrap();
        assert_eq!(first.permissions().mode() & 0o777, 0o444);
        prepare_webgpu_header(&header, &out_path, &expected_sum);
        let second = fs::metadata(&file).unwrap();
        assert_eq!((first.ino(), first.modified().unwrap()), (second.ino(), second.modified().unwrap()));

        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&file, b"corrupt header").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
        prepare_webgpu_header(&header, &out_path, &expected_sum);
        assert_eq!(fs::read(&file).unwrap(), contents);
        assert_eq!(fs::metadata(&header).unwrap().permissions().mode() & 0o777, 0o444);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn prepare_webgpu_header_rejects_override_mismatch_before_touching_cache() {
        let directory = test_directory("mismatch");
        let header = directory.join("override.h");
        let file = directory.join("webgpu.h");
        let contents = b"verified cached header";
        let expected_sum = sha256::digest(contents);
        fs::write(&header, b"invalid override").unwrap();
        fs::write(&file, contents).unwrap();

        let result = std::panic::catch_unwind(|| prepare_webgpu_header(&header, &directory, &expected_sum));
        assert!(result.is_err());
        assert_eq!(fs::read(&file).unwrap(), contents);
        fs::remove_dir_all(directory).unwrap();
    }
}
