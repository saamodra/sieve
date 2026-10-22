---
title: Responsible use
description: Where and how to use sieve safely.
---

Use sieve only on apps and devices you own or are explicitly authorized to test.

Anyone with the CA private key can read the HTTPS traffic of devices that trust it. Keep the key private, and run `sieve ca regenerate` if it may have been exposed.

Certificate-pinning bypass is meant for apps you own or are authorized to test.
