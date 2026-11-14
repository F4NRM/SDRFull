# Browser startup regression (#736 / #738)

The client now statically imports wasm-bindgen JavaScript below
snippets/sdroxide-ui-<hash>/inline0.js. The server used to flatten this URL to
/inline0.js, returning 404 before any WASM or WebSocket initialization.

Run the routing regression (without the fix both new tests fail):

    cargo test -p sdroxide-server --lib proxy_prefix_tests

Run the permission regression (Node >=22; no npm packages or hardware):

    node --test tools/web-startup/permissions.test.mjs

To prove the permission test fails with the reverted PR, set PTT_TEST_REV to
ae2f155e before running it. The first test observes two unintended prompts.

For an actual built-client check, build the web bundle with Trunk, then:

    cargo build -p sdroxide-server --example web_startup --features embed-web,rust-embed/debug-embed

Run target/debug/examples/web_startup (or the target-specific debug/examples path) with two arguments:
a free loopback port, followed by either the absolute crates/sdroxide-web/dist
directory (the --web-root path), or --embedded (the embedded asset path).
The debug-embed dependency feature forces actual embedded bytes even in a debug
build; otherwise rust-embed reads the source dist directory at runtime.
Set SDROXIDE_CONFIG_DIR to an empty scratch directory first. This example uses
the real server HTTP and WebSocket handlers but no radio engine or hardware.

Then run the smoke test with the absolute browser executable, URL and output PNG:

    node tools/web-startup/smoke.mjs <chrome> http://127.0.0.1:4959/ screenshot.png normal

Also test /shack/sdr/ and /?tab=controls. The final optional argument is normal,
denied (the default permission function rejects), or unsupported (all device APIs
are absent). Every scenario must receive a server WebSocket response, draw a
canvas, load all scripts/WASM without errors and request no device permission.

The script starts a fresh, temporary browser profile and closes only its own
Chrome process. It uses Chrome DevTools Protocol over loopback, with Node's
built-in WebSocket; it does not connect to the operator's browser. Screenshots
and temporary profiles are local test output and are not committed.

Serial/HID require HTTPS or localhost and a browser implementing those APIs.
Startup tests prove loading and permission isolation, not hardware PTT operation.
