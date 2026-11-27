# Third-party ideas

Ideas taken from other projects, each rewritten here rather than copied, with where it came from.

## victory-scarlett (Op-lite), GPL-3.0

- `VICTORY_FILL_ONCE` (`crates/victory-gpu/src/host.rs`, `HostList::fill_scratch`): a root constant buffer
  copied to a list's scratch once per list and per range, not once per draw. Idea from victory-scarlett commit
  `c96525dbb2`, which credits Belobug (84ea9c12) and romainhedouin.
