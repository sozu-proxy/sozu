# RPM Build Script

The `rpm_build.sh` script provides two ways to build sozu:

### Option 1 (build from sources)
First option, and default one, will build sozu using the current sources
in the root folder of this project. It will build whatever version is
currently checked out (but the `.rpm` file name will contain version set in `sozu.spec`)
regardless the source version.
```
./rpm_build.sh
```

### Option 2 (build release version)
The second option will build sozu by downloading from github the tagged
version defined in `sozu.spec`, also, it will *not* produce the debuginfo
packages.

If you're building for production, this option is recommended.

```
./rpm_build.sh --release
```
