+++
title = "Known issues"
weight = 80
+++


## Architectural limitations

Issues that are caused by design decisions of Garage internals, and that can't
be fixed without major architectural changes in the codebase.

### Buckets are not sharded

### Very big objects cause performance degradation

### No locking (`if-none-match`, ...)

### `CreateBucket` race condition



## Bugs

Known bugs that are complex to diagnose and fix, and therefore have not been
fixed yet.

### LMDB metadata corruption

### Layout updates might require manual intervention



## General footguns

Choices made by the developers that users must be aware of if they don't want
to run into potential issues.

### Resync tranquility is conservative by default
