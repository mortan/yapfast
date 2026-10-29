# macOS

Yapfast runs on Macs with Apple silicon, on macOS 14 or later; sharing other
apps' sound in Go Live needs 14.4. Intel Macs aren't built for. There's no
published package: `scripts/build-macos` builds the app on a Mac, as
[packaging](packaging.md#macos-app) describes. Like Windows, it comes in two
flavours: the release's Discord-only build, the default, and `full`, which
adds Matrix. Both are `Yapfast.app` with the bundle id
`io.github.yapfast.Yapfast`, and both keep one data folder,
`~/Library/Application Support/io.github.yapfast.Yapfast`.

## Build and run

`scripts/macos-env` takes the Nix shell's place, and needs Xcode or its
command line tools, Homebrew's `rustup`, `protobuf` and `llvm@21`, and `bun`
for the fake Discord; [the shells](building.md#shells) has what it sets up.

```sh
scripts/macos-env cargo run --locked -- --preview   # the UI without an account
scripts/macos-env cargo build --profile fast        # a development build
scripts/build-macos                                 # target/dist/Yapfast.app, Discord only
scripts/build-macos full                            # with Matrix
scripts/build-macos discord --dev                   # the same app, rebuilt in seconds; not for release
open target/dist/Yapfast.app
```

Opened from Finder or the Dock, Yapfast logs to
`~/Library/Logs/Yapfast/yapfast.log`, or with `--data-dir` to `yapfast.log`
in that folder, so each folder's Yapfast keeps its own, including why it
ended; run from a terminal, it logs there.
`YAPFAST_LOG=debug` adds its debug logs, as elsewhere.

## What works, and how

Yapfast does on macOS what it does on Linux, apart from what's
[open](#whats-open), through these parts.

- **Sound.** Discord calls, the input meter, ringing, Yapfast's sounds and a
  watched stream's or a video's sound go through Core Audio's HAL output
  unit, one per stream, and a ring between it and the caller; a stream fails
  and opens again as soon as Core Audio says its device went away. Yapfast
  (Matrix) calls play and record through WebRTC's own macOS device module.
  Devices are kept by their Core Audio UID
  ([architecture](architecture.md#how-a-ui-change-flows-through-the-code);
  [voice](voice.md#sounds) has how devices are named and followed).
- **Go Live's sound** is a Core Audio process tap of every process but
  Yapfast's own, from macOS 14.4 on; earlier, Yapfast shares without sound
  and logs why ([Go Live](discord/go-live.md)).
- **Screens and windows** are captured by libwebrtc: screens through
  ScreenCaptureKit, windows through Core Graphics' `CGWindowListCreateImage`.
  The share menu lists them itself, without macOS's picker, and leaves out
  Yapfast's own windows ([Go Live on macOS](discord/go-live.md#go-live-on-macos)).
- **Encoding.** Go Live's H264 and H265 go to VideoToolbox's hardware
  encoder, and to x264, with its Arm (NEON) assembly, when "Encode streams
  with the GPU" is off or VideoToolbox fails
  ([On macOS](discord/go-live.md#on-macos)). The camera encodes with x264.
- **Decoding.** VideoToolbox decodes watched H264 and H265 streams and video
  files' H264, and AudioToolbox video files' AAC sound, which libwebrtc's
  FFmpeg on macOS doesn't decode; less H264 plays than on Linux and Windows
  ([media libraries](building.md#media-libraries)).
- **Cameras** film through AVFoundation, behind the same C interface as
  libwebrtc's capture module on Linux and Windows
  ([cameras](voice.md#cameras)).
- **Push to talk** hears the key in every app through a listen-only
  `CGEventTap` ([push to talk](discord/voice.md#push-to-talk)).
- **The menu bar.** Closing the window leaves Yapfast's icon in the menu bar,
  with Open Yapfast, Disconnect and Quit; Cmd+Q and the Dock's Quit go
  through Yapfast's own Quit, and a call holds off App Nap
  ([the tray](tray.md#macos)).
- **Start at sign-in** writes a launchd agent,
  `~/Library/LaunchAgents/io.github.yapfast.Yapfast.plist`
  ([starting at sign-in](tray.md#starting-at-sign-in)).
- **Notifications** go through Apple's User Notifications framework, from
  `Yapfast.app` only (`src/notify.rs`).
- **Secrets** (the Discord token, the key of Discord's cache, Matrix
  sessions) go to the login Keychain through the `keyring` crate, under the
  service `io.github.yapfast.Yapfast`; see [Keychain prompts](#keychain-prompts).
- **Drawing.** Skia Software by default; "Render with the GPU" uses Skia
  OpenGL ([dependencies](building.md#dependencies)).
- **Keys.** Cmd stands in for Ctrl in Yapfast's shortcuts, zoom, copy and
  paste ([chat UI](chat-ui.md#layout-and-look)).
- **Memory.** Closing the window gives the allocator's free pages back with
  `malloc_zone_pressure_relief` ([the window](tray.md#the-window)).

## Permissions

macOS asks for each permission the first time Yapfast uses what needs it.
The sentences it shows come from the bundle's `Info.plist`.

| Permission | For | When macOS asks | Without it |
| --- | --- | --- | --- |
| Microphone | Calls and the input meter | The first time Yapfast opens a microphone | macOS hands over silence: nobody hears you, the meter stays still and says where to allow it, and the log says so as a call's microphone opens |
| Camera | Your camera in calls, Settings' camera test | The first time a camera opens | The camera fails to open, saying where to allow it; a first refusal from Settings' test or preview leaves it empty, and the next open says so |
| Screen & System Audio Recording | Go Live's screens and windows | The first time the share menu opens | The share menu stays shut, with a notice to allow it and then quit and reopen Yapfast, which macOS needs before it applies |
| System Audio Recording, in the same pane | Go Live's sound, through the process tap | The first time a stream's sound starts | macOS hands over silence, so the stream goes without sound; once other apps have played for 10 s with only silence heard, the log says so |
| Input Monitoring | Push to talk while another app is in front | When push to talk turns on | The key works only while Yapfast is in front, and settings say so; allowing it applies from the next start |
| Notifications | Message and call notifications | With the first notification | None show |

Accessibility is needed only by the ignored test
`the_tap_hears_the_key_go_down_and_up`, which types with `CGEventPost`.

Permissions belong to the app macOS holds responsible for the process. Started
as `Yapfast.app`, that's Yapfast, its stream workers included. A `yapfast`
binary run from a terminal, such as `target/fast/yapfast` or a test, is the
terminal's: macOS asks for the terminal app, and System Settings lists the
terminal, not Yapfast. Under tmux macOS refuses without asking, so run
anything that needs a permission from a terminal tab outside tmux, or open the
app. An unbundled binary shows no notifications at all; the framework needs
a bundle.

macOS ties each permission to the app's signature. An ad hoc signature
changes with every build, so each new build asks again; to keep the
permissions across builds, run `scripts/make-macos-signing-identity` once,
and the build signs with its stable identity from then on
([signing](packaging.md#macos-app)).
`tccutil reset All io.github.yapfast.Yapfast` forgets every answer, so macOS
asks again.

## Keychain prompts

Yapfast keeps its secrets in the login Keychain: for Discord the token
(`discord-token`) and the key of the cache it shows while connecting
(`discord-cache-key`); for Matrix each session's access token and its store
key (`ID-store-key`). The build that saves an item may read it without
asking. Another build, signed differently, makes macOS ask whether `yapfast`
may use the item, once per item: so after a rebuild, expect two prompts as a
linked Discord account starts. Yapfast reads the Discord token once a run,
not each time the sign-in screen shows. "Always Allow" adds that build to
the item; "Deny" leaves the item as it is: Yapfast says it couldn't read it
from the OS credential store, on the sign-in screen of a build without
Matrix rather than offering to link again, and macOS asks again on "Retry
saved session" or at the next start. A stable signing identity, as above,
stops these prompts too. Against the fake Discord
(`YAPFAST_DISCORD_TEST_SERVER`) Yapfast doesn't touch the Keychain.

## Checks

`scripts/check --fast` and `scripts/test-fake-discord` start through
`scripts/macos-env` on a Mac, as they start through `nix develop` on Linux
([checks](building.md#checks)); `scripts/warm-worktree` does too, cloning
the main checkout's build with APFS clones. The ignored device tests run as on
Linux, from a terminal that has the permission they need:

```sh
scripts/macos-env cargo test --profile fast -p yapfast-core opens_every_listed_device -- --ignored
scripts/macos-env cargo test --profile fast -p yapfast-core hears_other_applications -- --ignored   # System Audio Recording
scripts/macos-env cargo test --profile fast -p yapfast-core films_a_real_camera -- --ignored        # Camera
scripts/macos-env cargo test --profile fast -p yapfast push_to_talk -- --ignored                    # Input Monitoring, Accessibility
```

`hears_what_it_plays` needs an input that hears the default output, which a
Mac has only with a loopback device such as BlackHole.

These scripts are Linux-only: `scripts/ui-session` and
`scripts/run-framework-isolated`, which run Weston; `scripts/test-voice`,
`scripts/compare-noise-suppression` and `scripts/compare-room-echo`, which
enter the Nix shell; `scripts/measure-process`, which reads `/proc`;
`scripts/framework-input`, `scripts/build-framework-*`,
`scripts/install-desktop-entry`, `scripts/build-appimage` and
`scripts/package-arch`. `scripts/test-stack` needs Docker, which a Mac
doesn't come with.

## Measuring memory

Measure a `measure` build with the release's features, as on Linux
([how to measure](measurements/README.md#how-to-measure)):

```sh
scripts/macos-env cargo build --locked --profile measure --no-default-features --features desktop,discord --bin yapfast
footprint -p PID          # the process's footprint, as Activity Monitor's Memory column counts it
vmmap --summary PID       # by region, with the malloc zones' allocated and free sizes
```

`footprint --sample 1 --sample-duration 30 -p PID` samples it once a second.
Report the footprint, the macOS counterpart of Linux's `RssAnon` and Windows'
private working set. A stream worker, `yapfast --stream-worker`, is a process
of its own; measure it by its PID too. The `stats` command of
[measuring](measurements/README.md#how-to-measure) prints glibc's heap, so
it prints nothing on macOS; the malloc zones in `vmmap --summary` stand in
for it.

## What's open

- The local Matrix and LiveKit stack, `scripts/test-stack`, needs Docker.
- Discord Rich Presence is served only on Linux; games are still found among
  running programs ([Rich Presence](discord/people.md#linux-rich-presence)).
- Turning Yapfast off in System Settings › General › Login Items isn't read
  back, so Yapfast's own switch stays on ([starting at
  sign-in](tray.md#starting-at-sign-in)).
- Window capture goes through `CGWindowListCreateImage`, which Apple has
  deprecated in favour of ScreenCaptureKit.
- The app isn't notarized, whatever it's signed with, so a copy downloaded
  from elsewhere is stopped by Gatekeeper until allowed
  ([packaging](packaging.md#macos-app)).
