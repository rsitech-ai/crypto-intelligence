# Durable implementation goal

Implement the approved production specification end to end while preserving
every numerical, point-in-time, privacy, security, resource, governance, and
open-source requirement.

Loop:

1. select the next incomplete task;
2. define the behavioral contract;
3. implement the smallest complete behavior;
4. run all available checks;
5. review security, lineage, authority, and failure behavior;
6. create a DCO-signed checkpoint;
7. update progress and GitHub issue #1;
8. publish and verify the branch;
9. continue until stable-release evidence is complete.

Unavailable CI or platform tooling never halts source work, but it is never
represented as passing.
