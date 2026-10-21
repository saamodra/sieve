//! `sieve ca` subcommands, run as the built binary against a tempdir config.

mod common;

use std::{fs, path::Path, process::Command};

use common::{run_sieve, run_sieve_ok};
use tempfile::TempDir;
use x509_parser::{parse_x509_certificate, pem::parse_x509_pem};

fn parse_der(der: &[u8]) -> x509_parser::certificate::X509Certificate<'_> {
    let (rest, cert) = parse_x509_certificate(der).expect("DER certificate parses");
    assert!(rest.is_empty(), "trailing bytes after the certificate");
    cert
}

/// A config dir that already holds a CA. The run that creates a CA also logs
/// to stdout, which is a bug of its own (see `first_run_keeps_stdout_clean`),
/// so the tests create the CA here and throw that output away.
fn config_with_ca() -> TempDir {
    let dir = TempDir::new().unwrap();
    run_sieve(dir.path(), &["ca", "info"]);
    dir
}

fn openssl_available() -> bool {
    Command::new("openssl")
        .arg("version")
        .output()
        .is_ok_and(|out| out.status.success())
}

#[test]
fn path_prints_files_inside_config_dir() {
    let dir = TempDir::new().unwrap();
    let stdout = run_sieve_ok(dir.path(), &["ca", "path"]);
    let lines: Vec<&Path> = std::str::from_utf8(&stdout)
        .unwrap()
        .lines()
        .map(Path::new)
        .collect();

    assert_eq!(lines.len(), 2, "expected the certificate and the key");
    assert_eq!(lines[0], dir.path().join("ca.pem"));
    assert_eq!(lines[1], dir.path().join("ca.key"));
}

#[test]
fn export_pem_parses_as_a_ca_certificate() {
    let dir = config_with_ca();
    let pem = run_sieve_ok(dir.path(), &["ca", "export", "--format", "pem"]);

    let (_, block) = parse_x509_pem(&pem).expect("PEM block parses");
    let cert = block
        .parse_x509()
        .expect("PEM contains an X.509 certificate");
    assert!(cert.is_ca(), "exported certificate is not a CA");
}

#[test]
fn export_der_parses_and_matches_pem() {
    let dir = config_with_ca();
    let pem = run_sieve_ok(dir.path(), &["ca", "export", "--format", "pem"]);
    let der = run_sieve_ok(dir.path(), &["ca", "export", "--format", "der"]);

    let (_, block) = parse_x509_pem(&pem).unwrap();
    assert_eq!(block.contents, der, "DER export differs from the PEM body");
    assert!(parse_der(&der).is_ca());
}

#[test]
fn export_cer_is_der() {
    let dir = config_with_ca();
    let der = run_sieve_ok(dir.path(), &["ca", "export", "--format", "der"]);
    let cer = run_sieve_ok(dir.path(), &["ca", "export", "--format", "cer"]);

    assert_eq!(cer, der);
    assert!(parse_der(&cer).is_ca());
}

#[test]
fn export_writes_to_out_file() {
    let dir = config_with_ca();
    let out = dir.path().join("sieve-ca.crt");
    run_sieve_ok(
        dir.path(),
        &[
            "ca",
            "export",
            "--format",
            "der",
            "--out",
            out.to_str().unwrap(),
        ],
    );

    let written = fs::read(&out).expect("export wrote the file");
    assert!(parse_der(&written).is_ca());
}

#[test]
fn hash_matches_openssl_subject_hash_old() {
    if !openssl_available() {
        eprintln!("skipping: openssl is not installed");
        return;
    }
    let dir = config_with_ca();
    let pem_path = dir.path().join("exported.pem");
    let pem = run_sieve_ok(dir.path(), &["ca", "export", "--format", "pem"]);
    fs::write(&pem_path, pem).unwrap();

    let ours = run_sieve_ok(dir.path(), &["ca", "hash"]);
    let openssl = Command::new("openssl")
        .args(["x509", "-subject_hash_old", "-noout", "-in"])
        .arg(&pem_path)
        .output()
        .expect("running openssl");
    assert!(
        openssl.status.success(),
        "openssl failed: {}",
        String::from_utf8_lossy(&openssl.stderr)
    );

    assert_eq!(
        String::from_utf8_lossy(&ours).trim(),
        String::from_utf8_lossy(&openssl.stdout).trim()
    );
}

#[test]
fn second_run_reuses_the_same_ca() {
    let dir = config_with_ca();
    let first = run_sieve_ok(dir.path(), &["ca", "export", "--format", "der"]);
    let key_first = fs::read(dir.path().join("ca.key")).unwrap();

    let second = run_sieve_ok(dir.path(), &["ca", "export", "--format", "der"]);
    let key_second = fs::read(dir.path().join("ca.key")).unwrap();

    assert_eq!(first, second, "a second run generated a new certificate");
    assert_eq!(key_first, key_second, "a second run rewrote the key");
}

#[test]
fn regenerate_replaces_the_ca() {
    let dir = config_with_ca();
    let before = run_sieve_ok(dir.path(), &["ca", "export", "--format", "der"]);
    let hash_before = run_sieve_ok(dir.path(), &["ca", "hash"]);

    run_sieve_ok(dir.path(), &["ca", "regenerate"]);

    let after = run_sieve_ok(dir.path(), &["ca", "export", "--format", "der"]);
    let hash_after = run_sieve_ok(dir.path(), &["ca", "hash"]);
    assert_ne!(before, after, "regenerate kept the old certificate");
    assert!(parse_der(&after).is_ca());
    assert_ne!(hash_before, hash_after);
}

#[test]
fn first_run_keeps_stdout_clean() {
    let dir = TempDir::new().unwrap();
    let der = run_sieve_ok(dir.path(), &["ca", "export", "--format", "der"]);
    assert!(parse_der(&der).is_ca());
}
