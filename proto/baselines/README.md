# Protobuf Compatibility Baselines

`cmti-v1.binpb` is the first complete, independently reviewed compatibility
baseline for all seven `cmti.*.v1` packages. It does not claim compatibility
with the earlier excluded scaffold files. New changes must pass:

```sh
buf breaking proto --against proto/baselines/cmti-v1.binpb
```

The baseline is a deterministic `FileDescriptorSet` produced by:

```sh
buf build proto --as-file-descriptor-set \
  --output proto/baselines/cmti-v1.binpb
```

Its SHA-256 is recorded in `cmti-v1.sha256`.

## Compatibility matrix status

The current-app/current-daemon contract is covered by the Rust loopback,
daemon-process, and native Swift client tests. Unknown optional fields and
future enum values are exercised in both generated runtimes. Stream resume,
message-size, unauthenticated-session, and expired-session behavior also have
behavioral tests.

There is no previous supported daemon or app minor yet, so
current-app/previous-daemon and previous-app/current-daemon are not applicable
to this first baseline. Those two matrix legs become mandatory when a second
supported minor is introduced; CI must use the retained prior generated client
and daemon artifacts rather than a fabricated same-version fixture.
