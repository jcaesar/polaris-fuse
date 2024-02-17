# Polaris fuse mounter

Mount a polaris server as a local folder, to be used with any media player.

## Usage Example
```
$ mount-polaris ./polaris https://demo.polaris.stream demo_user <(echo demo_password)
$ mpv ./polaris/search/cyber
[file] This is a directory - adding to playlist.
Playing: sshfs/search/cyber/Chris Zabriskie - Abandon Babylon - 03 Where Have All the Cybertrackers Gone-.mp3
```
## Installation
* Via cargo: `cargo install --git https://github.com/jcaesar/polaris-fuse/`
* Via nix: `nix shell github:jcaesar/polaris-fuse`

## Caveats
* Implementation is rough
  * Mostly single-threaded
  * No proper caching
  * No proper unmounting
* FUSE and polaris aren't a good fit
  * No stable IDs that could be used as Inodes
  * FUSE needs to know the file size to even return it in a directory list,
    polaris doesn't offer a way of getting the size without requesting the file
