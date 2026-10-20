//! Generators for the configuration profile and the PAC file.

use std::net::SocketAddr;

use base64::{Engine, engine::general_purpose::STANDARD};
use uuid::Uuid;

const IDENTIFIER: &str = "dev.sieve.ca";

/// An unsigned profile that installs `ca_der` as a root certificate.
pub fn mobileconfig(ca_der: &[u8]) -> String {
    let data = STANDARD.encode(ca_der);
    let cert_uuid = Uuid::new_v4().to_string().to_uppercase();
    let profile_uuid = Uuid::new_v4().to_string().to_uppercase();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>PayloadContent</key>
	<array>
		<dict>
			<key>PayloadCertificateFileName</key>
			<string>sieve-ca.cer</string>
			<key>PayloadContent</key>
			<data>{data}</data>
			<key>PayloadDescription</key>
			<string>Adds the sieve root certificate</string>
			<key>PayloadDisplayName</key>
			<string>sieve CA</string>
			<key>PayloadIdentifier</key>
			<string>{IDENTIFIER}.cert</string>
			<key>PayloadType</key>
			<string>com.apple.security.root</string>
			<key>PayloadUUID</key>
			<string>{cert_uuid}</string>
			<key>PayloadVersion</key>
			<integer>1</integer>
		</dict>
	</array>
	<key>PayloadDescription</key>
	<string>Lets sieve inspect HTTPS traffic from this device</string>
	<key>PayloadDisplayName</key>
	<string>sieve CA</string>
	<key>PayloadIdentifier</key>
	<string>{IDENTIFIER}</string>
	<key>PayloadOrganization</key>
	<string>sieve</string>
	<key>PayloadRemovalDisallowed</key>
	<false/>
	<key>PayloadType</key>
	<string>Configuration</string>
	<key>PayloadUUID</key>
	<string>{profile_uuid}</string>
	<key>PayloadVersion</key>
	<integer>1</integer>
</dict>
</plist>
"#
    )
}

/// PAC script sending everything through the proxy at `addr`.
pub fn pac(addr: SocketAddr) -> String {
    format!("function FindProxyForURL(url, host) {{\n  return \"PROXY {addr}; DIRECT\";\n}}\n")
}
