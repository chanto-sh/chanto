# block-buffer advisory applicability

`GHSA-qwgh-2vcv-g2f7` reports a caught-panic cursor-corruption bug and lists
all versions below 0.12.1 as affected. The current lockfiles contain 0.10.4
through `sha2` 0.10 dependencies, including the desktop webview, alongside
patched 0.12.1 through newer hashing dependencies.

## Why the 0.10.4 finding does not apply

The advisory's mechanism requires storing the cursor inside the same block
that receives input data. A panicking callback can leave that cursor byte
corrupted before it is restored.

The reviewed 0.10.4 implementation has a separate `pos: u8` field. Its
`digest_blocks` callback receives immutable blocks and cannot overwrite that
field. `ReadBuffer`, the advisory's other affected type, does not exist in
0.10.4. This is an implementation distinction, not an assumption that Chanto
will never catch panics.

Sources:

- [Upstream advisory and reproducer](https://github.com/RustCrypto/utils/security/advisories/GHSA-qwgh-2vcv-g2f7)
- [0.10.4 source](https://github.com/RustCrypto/utils/blob/block-buffer-v0.10.4/block-buffer/src/lib.rs)
- [Upstream fix for the newer implementation](https://github.com/RustCrypto/utils/pull/1487)

## Reproduction

On October 26, 2026, the advisory's EagerBuffer reproducer was adapted to
0.10.4's `generic_array` type import and run against the registry release:

```toml
[dependencies]
block-buffer = "=0.10.4"
```

```rust
use block_buffer::{generic_array::typenum::U4, EagerBuffer};

fn main() {
    let mut buf = EagerBuffer::<U4>::new(&[1, 2]);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        buf.digest_blocks(&[3, 0xff], |_| panic!("compression failure"));
    }));
    assert!(caught.is_err());
    assert_eq!(buf.get_pos(), 2);
    assert_eq!(buf.get_data(), &[1, 2]);
    buf.reset();
    buf.digest_blocks(&[5, 6], |_| {});
    assert_eq!(buf.get_pos(), 2);
    assert_eq!(buf.get_data(), &[5, 6]);
}
```

All assertions passed. This was a normal Rust execution, not a Miri run;
the separate-field source review supplies the applicability rationale.

## Exception boundary

Both OSV configs ignore only this advisory ID, so other advisories for the
same package still report. OSV's advisory-ID exceptions cannot select a
package version. `scripts/check-advisory-exceptions.py` therefore validates
both lockfiles before the required OSV scan and in a separate prerequisite
job before published Scorecard analysis. The publishing job itself retains
the restricted steps required by Scorecard's result-integrity policy.

Only registry 0.10.4 with its reviewed checksum, or stable versions at least
0.12.1, pass. Other older versions, changed checksums and prereleases fail
closed and need reassessment. Tests cover both lockfiles and these cases.
Remove the exception when upstream dependencies no longer need 0.10.4 or
the advisory's affected range is corrected. Do not lower the Scorecard floor.
