# OpenBikeNet Server

**Status: not started. There is no code here yet.**

This will be the reference server for the [OpenBikeNet protocol](https://github.com/OpenBikeNet/protocol):
a shared, neutral server that lets riders of different indoor cycling apps see each other on
the same route. See the [organisation profile](https://github.com/OpenBikeNet) for the overall goal.

The protocol is designed first, in the [protocol repository](https://github.com/OpenBikeNet/protocol).
Code here follows once the first draft of the specification is agreed. Ideas and feedback are
welcome in the protocol repository's Discussions.

## Planned

- **Shared presence first:** apps report their riders' positions on shared routes and receive
  the positions of riders from other apps. Each app keeps its own physics.
- **Self-hostable:** anyone can run their own instance. A public instance is planned as a
  community service.
- **Minimal data:** only what riding together needs, nothing stored that isn't required.

## License

[GNU Affero General Public License v3.0](LICENSE). If you run a modified version as a public
service, you share your changes. Apps that only talk to the server over the protocol are not
affected and can use any license.
