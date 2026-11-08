# Vendored libraries

Plain ES modules with no build step, so third-party code is checked in as a single bundle.

| File | What | Version | Licence |
| --- | --- | --- | --- |
| `sendspin-js-6.0.0.js` | `@sendspin/sendspin-js`, the Sendspin player Pinson runs in the browser to play as a synced speaker (exports `SendspinPlayer`) | 6.0.0 | Apache-2.0 (`LICENSE-sendspin-js.txt`) |

Bundled into it (esbuild, minified):

| Package | Version | Licence |
| --- | --- | --- |
| `@noble/ciphers` | 2.4.0 | MIT |
| `@noble/curves` | 2.4.0 | MIT |
| `@noble/hashes` | 2.4.0 | MIT |
| `opus-encdec` | 0.1.1 | MIT |

The MIT licence requires its notice to travel with the code: the notices are in `LICENSE-bundled-mit.txt`.

Rebuild with `./build-sendspin.sh` (needs node and npm). 6.0.0 is pinned on purpose: nidoo-sound sends a
13-byte audio header and 5.0.0 reads 9, which crackles.
