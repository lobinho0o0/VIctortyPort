fn main() {
    if std::env::var("CARGO_CFG_TARGET_ABI").as_deref() == Ok("llvm") {
        println!("cargo:rustc-link-arg-cdylib=-fuse-ld=lld");
        println!("cargo:rustc-link-arg-cdylib=-Wl,--delayload=victory-probe-delayed.dll");
    } else {
        println!("cargo:rustc-link-arg-cdylib=/DELAYLOAD:victory-probe-delayed.dll");
    }
    println!("cargo:rustc-link-lib=delayimp");
}
