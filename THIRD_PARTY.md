# Third-party ideas

Ideas taken from other projects, each rewritten here rather than copied, with where it came from.

## victory-scarlett (Op-lite), GPL-3.0

- `VICTORY_FILL_ONCE` (`crates/victory-gpu/src/host.rs`, `HostList::fill_scratch`): a root constant buffer
  copied to a list's scratch once per list and per range, not once per draw. Idea from victory-scarlett commit
  `c96525dbb2`, which credits Belobug (84ea9c12) and romainhedouin.
- `VICTORY_SOFT_VIEWS` (`crates/victory-gpu/src/softview.rs`; hooks in `crates/victory-hle/src/modules/xmem.rs`
  `map_alias`, `crates/victory-gpu/src/texture.rs`): the views the title's page-mapping batch copies make are kept in
  the books and read through a lasting view of the whole section, a window, instead of a map and unmap call per
  64 KiB page; `=verify` checks the window's bytes against the mapped views. Idea from victory-scarlett branch
  `scarlett-fox`, `crates/victory-hle/src/modules/xmem.rs` (`mod soft`, `VICTORY_SOFT_VIEWS=copies`), read on
  2026-11-26 (commit and author of that change not recorded); written anew here.
