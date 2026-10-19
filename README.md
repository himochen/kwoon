# kwoon

kwoon (館, Cantonese for a training hall) is a governance layer for AI coding agents.
Agents propose changes. kwoon records the evidence behind each change, runs the gates,
and lets a change merge only under rules that a human approved.

## Status

Pre-MVP. Nothing here is usable yet. The design lands first, as reviewed documents:

- `docs/specs/`: decision records. A spec is frozen once its tracking issue first closes;
  a later change gets a new spec with a `supersedes:` header.
- `docs/plans/`: work packages. A plan is reviewed in a pull request, frozen once merged,
  and deleted when its tracking issue closes.

The first version targets GitHub only.

## License

[Apache-2.0](LICENSE)
