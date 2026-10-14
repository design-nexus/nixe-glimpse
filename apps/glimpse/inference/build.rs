fn main() {
    let root = std::path::PathBuf::from(
        std::env::var("GLIMPSE_LLAMA_DIR").expect("set GLIMPSE_LLAMA_DIR to the built llama.cpp prefix"),
    );
    cc::Build::new()
        .cpp(true)
        .file("native/embedding.cpp")
        .include(root.join("include"))
        .flag_if_supported("-std=c++17")
        .compile("glimpse_embedding");
    println!("cargo:rustc-link-search=native={}", root.join("lib").display());
    for library in ["mtmd", "llama", "ggml", "ggml-base"] {
        println!("cargo:rustc-link-lib=dylib={library}");
    }
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", root.join("lib").display());
    println!("cargo:rerun-if-env-changed=GLIMPSE_LLAMA_DIR");
    println!("cargo:rerun-if-changed=native/embedding.cpp");
}
