# Third-party ideas

Ideas taken from other projects, each rewritten here rather than copied, with where it came from.

## victory-scarlett (Op-lite), GPL-3.0

- `VICTORY_FILL_ONCE` (`crates/victory-gpu/src/host.rs`, `HostList::fill_scratch`): a root constant buffer
  copied to a list's scratch once per list and per range, not once per draw. Idea from victory-scarlett commit
  `c96525dbb2`, which credits Belobug (84ea9c12) and romainhedouin.

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
