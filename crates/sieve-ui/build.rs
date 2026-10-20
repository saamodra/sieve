fn main() {
    // The frontend is embedded from `ui/dist`; rebuild when it is (re)built.
    println!("cargo:rerun-if-changed=../../ui/dist");
}
