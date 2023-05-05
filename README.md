`roracache` (Read-Only Read-Ahead CACHE) is a FUSE filesystem that provides a cached version of some other part of your filesystem tree.

tl;dr if you have a really slow network share `/mnt/bigbox`, you can do `roracache /mnt/bigbox /mnt/bigbox_fast`, and any accesses to `/mnt/bigbox_fast` will be the same as accesses to `/mnt/bigbox`, except that there will be a lot of aggressive caching and readahead.

`roracache` is specifically designed for having a media collection on a network share, and not wanting to put up with various file dialogs etc. being unbearably slow, or with your media playback software never wanting to buffer enough data to withstand a short connectivity blip without misbehaving in other ways. (Looking at you, VLC.)

If the standard output is a terminal, `roracache` will use [`liso`](https://github.com/SolraBizna/liso) to display a friendly status display, showing what files are open and what their buffer status is. If you don't want this, redirect standard output to `/dev/null`.

# Requirements

- Rust compiler
- FUSE
    - **FreeBSD**: `fusefs` comes with your system, but may need to be added to your `loader.conf`.
    - **Linux**: FUSE almost always comes with your Linux system. (If it doesn't, you probably already know how to fix this.)
    - **macOS**: Get [macFUSE](https://osxfuse.github.io/).
    - **OpenBSD**: `fuse` should be built in.
    - **Windows**: Sorry, by its very nature FUSE requires a UNIX system.

# Installation

```sh
cargo install roracache
```

# Caveats

- `roracache` **does not support writes**. It's a read-only cache.
- `roracache` **is very easily confused if files substantially change identity** e.g. by being added, removed, resized, rewritten. It's especially bad at noticing when a file's contents change. `roracache` will notice most kinds of changes within a few minutes, but in particular it will *never* notice if a symlink target changes. (If it's not noticing changes fast enough, simply control-C and re-run `roracache`.)
- `roracache` **flattens all UNIX permissions**. Underlying files or directories with any execute bit set are seen as `r-xr-xr-x`, and all other files or directories are seen as `r--r--r--`. All files will be owned by the uid and gid of the `roracache` process.
- `roracache` **does not support extended attributes**, or the extra file attributes found on macOS. All extended attributes on the underlying files are simply ignored.
- `roracache` **will waste up to ¾GiB of anonymous memory per open file**, though never more than the size of the file. (Whether this memory is actually wasted is debatable, given that aggressive caching is the whole point of `roracache`. It's definitely annoying that this amount isn't configurable.)
- `roracache` **completely makes up inode numbers**. Inode 1 is the root of the mount, and inode numbers are made up sequentially from there.
- `roracache` **ignores filesystem boundaries**.
- `roracache` **does not support special files**. (It will do files, directories, and symlinks. That's it. All other kinds of inode will be censored.)
- `roracache` **does not cache errors**. This *should* mean that error cases are exactly as slow as with the underlying filesystem, while allowing quick recovery from "oops I forgot to mount my media share" type mistakes.
- `roracache` **will panic if a read or seek fails**. No excuses; this is me being lazy. Sorry.
- `roracache` **wastes some CPU time** if the status display is used. The amount of CPU time wasted is tiny compared to the amount of IO, or compared to decoding the media you're caching, but this might be a concern on a very weak device.
- `roracache` **does a lot of unnecessary memory copying**. There's not much I can do about this. Some of the extra copies are an unavoidable consequence of the design of the Rust binding to FUSE, and another is an unavoidable consequence of a design flaw in FUSE.
- `roracache` **has poor best-case performance**. On my test machines, it only pulled about a hundred megabits. This should be enough for all but the most obscenely overkill media files. It didn't use much CPU to do that, at least. It spent most of its time waiting for IO. (Note: Its worst-case performance in the face of overloaded CPU or slow IO is excellent, which is the *entire point*.)
- `roracache` **makes no attempt to cooperate between multiple opens on the same file**. The intended use case will have only one file open the majority of the time. The less your use case looks like that one, the better served it would be by your OS's built-in buffer cache anyway.
- `roracache` **can only buffer a contiguous region of a file**. If you seek forward and backward rapidly enough, it will do a lot of extra IO. The impact should be pretty minimal, since it only performs readaheads when it isn't doing other IO, and prefers to do readahead on whichever file is closest to running out of buffer at the moment.
- `roracache`'s source code **is ugly and hard to understand**. The whole thing was written as a present to someone else for my birthday, largely late at night.

# Legalese

`roracache` is copyright 2023, Solra Bizna, and licensed under either of:

 * Apache License, Version 2.0
   ([LICENSE-APACHE](LICENSE-APACHE) or
   <http://www.apache.org/licenses/LICENSE-2.0>)
 * MIT license
   ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the `roracache` crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
