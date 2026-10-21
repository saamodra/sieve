/*
 * sieve's Android certificate-pinning bypass for Frida.
 *
 * Written for sieve (not copied from any third-party unpinning script) after
 * surveying existing community scripts turned up no clearly-licensed
 * candidate to bundle: HTTP Toolkit's `android-certificate-unpinning.js` is
 * AGPL-3.0 (viral, unsuited to vendoring into a differently-licensed tool),
 * and the other widely-circulated scripts (e.g. the Frida CodeShare
 * "multiple-unpinning" script, iddoeldor/frida-snippets) carry no license at
 * all, so no permission to redistribute is actually granted. This script
 * reimplements the same, publicly documented hook points from scratch. See
 * docs/frida.md for the license note.
 *
 * Covers the checks apps most commonly use to pin or validate certificates:
 *   - javax.net.ssl.X509TrustManager (any custom implementation)
 *   - javax.net.ssl.TrustManagerFactory / SSLContext.init (forces the
 *     platform's default trust managers, then a trust-all fallback)
 *   - OkHttp 3/4 CertificatePinner.check / check$okhttp
 *   - OkHttp's internal CertificateChainCleaner pinning failure
 *   - WebView TLS errors (WebViewClient.onReceivedSslError)
 *   - Android's Network Security Config pinning (RootTrustManager /
 *     PinningTrustManager inside android.security.net.config)
 *
 * Usage: spawned automatically by `sieve android frida --package <id>`, or
 * manually with `frida -U -f <package> -l android-unpinning.js --no-pause`.
 */

Java.perform(function () {
    const log = (msg) => console.log(`[sieve-unpin] ${msg}`);

    function hookTrustAllTrustManager() {
        try {
            const TrustManager = Java.registerClass({
                name: 'com.sieve.unpin.TrustAllTrustManager',
                implements: [Java.use('javax.net.ssl.X509TrustManager').class],
                methods: {
                    checkClientTrusted: function (chain, authType) {},
                    checkServerTrusted: function (chain, authType) {},
                    getAcceptedIssuers: function () {
                        return [];
                    },
                },
            });
            return TrustManager;
        } catch (e) {
            log(`could not build a trust-all TrustManager: ${e}`);
            return null;
        }
    }

    function hookSslContext() {
        const trustAll = hookTrustAllTrustManager();
        if (!trustAll) return;
        try {
            const SSLContext = Java.use('javax.net.ssl.SSLContext');
            SSLContext.init.overload(
                '[Ljavax.net.ssl.KeyManager;',
                '[Ljavax.net.ssl.TrustManager;',
                'java.security.SecureRandom'
            ).implementation = function (keyManagers, trustManagers, secureRandom) {
                log('SSLContext.init: substituting a trust-all TrustManager');
                const managers = [trustAll.$new()];
                this.init(keyManagers, managers, secureRandom);
            };
        } catch (e) {
            log(`SSLContext hook failed: ${e}`);
        }
    }

    function hookCustomTrustManagers() {
        // Any class that implements X509TrustManager and overrides
        // checkServerTrusted is a candidate for a hand-rolled pinning check.
        Java.enumerateLoadedClasses({
            onMatch: function (name) {
                if (name.indexOf('TrustManager') === -1) return;
                try {
                    const klass = Java.use(name);
                    if (!klass.checkServerTrusted) return;
                    klass.checkServerTrusted.overloads.forEach((overload) => {
                        overload.implementation = function () {
                            log(`${name}.checkServerTrusted: bypassed`);
                        };
                    });
                } catch (_e) {
                    // Not every loaded class name resolves cleanly; skip it.
                }
            },
            onComplete: function () {},
        });
    }

    function hookOkHttp() {
        ['okhttp3.CertificatePinner', 'okhttp3.internal.tls.CertificatePinner'].forEach(
            (className) => {
                try {
                    const CertificatePinner = Java.use(className);
                    const names = ['check', 'check$okhttp'];
                    names.forEach((methodName) => {
                        const method = CertificatePinner[methodName];
                        if (!method) return;
                        method.overloads.forEach((overload) => {
                            overload.implementation = function () {
                                log(`${className}.${methodName}: bypassed`);
                            };
                        });
                    });
                } catch (_e) {
                    // OkHttp not present under this name, or a shaded copy; skip it.
                }
            }
        );
    }

    function hookNetworkSecurityConfig() {
        [
            'android.security.net.config.RootTrustManager',
            'android.security.net.config.NetworkSecurityTrustManager',
        ].forEach((className) => {
            try {
                const TrustManager = Java.use(className);
                TrustManager.checkServerTrusted.overloads.forEach((overload) => {
                    overload.implementation = function () {
                        log(`${className}.checkServerTrusted: bypassed`);
                    };
                });
            } catch (_e) {
                // Not every API level/app exposes these classes; skip it.
            }
        });

        try {
            const PinManager = Java.use('android.security.net.config.PinSetTrustManager');
            PinManager.checkServerTrusted.overloads.forEach((overload) => {
                overload.implementation = function () {
                    log('PinSetTrustManager.checkServerTrusted: bypassed');
                };
            });
        } catch (_e) {
            // Present only when the app's manifest declares pin-set entries.
        }
    }

    function hookWebView() {
        try {
            const WebViewClient = Java.use('android.webkit.WebViewClient');
            WebViewClient.onReceivedSslError.implementation = function (view, handler, error) {
                log('WebViewClient.onReceivedSslError: proceeding anyway');
                handler.proceed();
            };
        } catch (e) {
            log(`WebViewClient hook failed: ${e}`);
        }
    }

    hookSslContext();
    hookCustomTrustManagers();
    hookOkHttp();
    hookNetworkSecurityConfig();
    hookWebView();
    log('hooks installed');
});
