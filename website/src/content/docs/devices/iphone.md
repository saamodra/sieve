---
title: iPhone
description: Intercept traffic from a physical iPhone over Wi-Fi.
---

```sh
sieve ios-device setup
```

1. Run the command and scan the QR code with the iPhone camera, or open the setup URL it prints.
2. Install the profile, then enable it under **Settings → General → About → Certificate Trust Settings**.
3. Set the Wi-Fi proxy to **Automatic** with the PAC URL shown on the setup page.

The setup page shows a green check once HTTPS traffic arrives.

:::caution[LAN exposure]
`ios-device setup` makes the proxy and the web UI listen on your local network until you press Ctrl+C. The UI API requires the token printed in the terminal. Run it on a network you trust.
:::

The Mac and the iPhone must be on the same Wi-Fi network.
