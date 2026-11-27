# Third-party ideas

Ideas taken from other projects, each rewritten here rather than copied, with where it came from.

## victory-scarlett (Op-lite), GPL-3.0

- `VICTORY_FILL_ONCE` (`crates/victory-gpu/src/host.rs`, `HostList::fill_scratch`): a root constant buffer
  copied to a list's scratch once per list and per range, not once per draw. Idea from victory-scarlett commit
  `c96525dbb2`, which credits Belobug (84ea9c12) and romainhedouin.
- `VICTORY_TABLE_MEMO` (`crates/victory-gpu/src/draw.rs`, `State::table_entry`): a table entry a list made already,
  with the same descriptor bytes, read the same way and nothing changed in the list since, is not made again; `=2`
  verifies. Idea from victory-scarlett commit `c45b43e96a`; written anew here.
- `VICTORY_SOFT_VIEWS` (`crates/victory-gpu/src/softview.rs`; hooks in `crates/victory-hle/src/modules/xmem.rs`
  `map_alias`, `crates/victory-gpu/src/texture.rs`): the views the title's page-mapping batch copies make are kept in
  the books and read through a lasting view of the whole section, a window, instead of a map and unmap call per
  64 KiB page; `=verify` checks the window's bytes against the mapped views. Idea from victory-scarlett branch
  `scarlett-fox`, `crates/victory-hle/src/modules/xmem.rs` (`mod soft`, `VICTORY_SOFT_VIEWS=copies`), read on
  2026-11-26: commit `bc264a4174` (author "scarlett-local", 2026-11-27); written anew here.

## JMAN730/project-flamingo, GPL-3.0

- `VICTORY_TEXTURE_BACK` (`crates/victory-gpu/src/texback.rs`, hooks in `HostList::texture_written` /
  `textures_back` and `read_backs`): textures shaders write that the title's CPU code reads (the ocean's height and
  displacement maps boats float on) are copied back into title memory, in the console's layout, once their list
  has run. Rewritten here after commit `cdc03a8fe32f` (fever308), which ports fxceless pull request 999 with that
  fork's later fixes (only the subresources a view writes, no unaddressable layouts, the texture's state kept).

## fxceless/victory-fork, GPL-3.0

- `VICTORY_TEXTURE_BACK`, as above: the idea and the read watch that found ~30 title addresses reading those
  texture kinds come from pull request 999 by Geckomacabre (commits `895c0b8f7`, `04df43508`, merged as
  `f5c03f3b`). Rewritten here, not copied.
