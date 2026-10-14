# ENG-194 — Biome ambience and audio inventory

The sandbox client now follows the local player's biome in the replicated climate field and streams the selected game-owned Ogg ambience. The audio device, file open, and decoder live on a worker thread; a bounded latest-change channel keeps the render loop non-blocking. Two sinks crossfade over 900 ms. An unchanged biome reuses the loaded sink; invalid/empty/out-of-bounds climate data, Snow, Scree, and missing files resolve to silence. The player listener follows the free camera while flying.

Audio is best-effort. Device initialization or decode errors are logged and gameplay continues. `SPALL_AUDIO=0` disables ambience; `SPALL_AUDIO_VOLUME=0..1` sets gain (default 0.2). Weather assets are catalogued and packaged for future work but are never selected by the client. The gravel clip is foley and is not automatically played.

## Biome selection

| Biome | Default layer | Notes |
|---|---|---|
| Meadow | Meadow Pipit call | Sparse bird recording; no continuous grass bed was accepted. |
| Alpine | Common Chiffchaff call | Recorded in alpine meadow. |
| TemperateForest | Woodland ambience | Author describes birds, wind, insects and crows; notes microphone noise. |
| BorealForest, PineForest | Swedish pine canopy wind | Source describes 25 m/s wind; use conservatively at the default ambience gain. |
| Steppe, Desert | Dry grass in wind | CC0 recording from the Chilean desert; shared dry open-country layer. |
| Ocean | Open ocean waves | Rockaway Beach surf recording. |
| Beach | Shore waves | Gentle Jersey shore recording. |
| Riparian | Flowing stream | Fast-flowing creek recording; the accepted Lake Ontario clip remains a separately named water ambience asset for later lake-specific selection. |
| Swamp | Wetland ambience | RyanKingArt recording; a Camargue sunset swamp recording is retained as an alternate. |
| Snow, Scree | Silence | No accepted natural Snow recording; gravel footfalls are foley and not a Scree ambience. |

The climate raster is expanded only when its compressed field changes. Lookups run at most four times per second and explicitly reject empty fields, malformed fields, non-finite positions, and coordinates outside the raster before sampling; `CoverGrid::at` otherwise clamps out-of-range positions to an edge biome.

## Remaining sound needs

- Quiet meadow and ordinary evergreen wind beds; the accepted pine recording describes storm-strength 25 m/s wind.
- Natural snowfield wind and high-altitude scree ambience. The available snowstorm candidate was designed as an indoor effect and was not downloaded; gravel steps do not fill this ambience gap.
- A calm inland-lake recording. The accepted Lake Ontario waves are retained as a named alternate, while the default Riparian pad uses the accepted flowing creek.
- Additional seasonal bird/insect layers and water/weather blending. The approved weather files are ready in the catalogue, but weather state and playback are outside this change.
- A listening pass for all 19 originals and converted copies, including loop points, noise floor, relative loudness, and stereo placement.

## Accepted recordings and provenance

The 19 files listed in `artifacts/audio-research/download-audit.json` are the accepted originals. Their bytes remain unchanged in that archive. Runtime copies are named by use and encoded as Vorbis to reduce package size. The table records the creator/source and license; the source pages or license deeds provide the full terms.

| Runtime category/name | Source and attribution | License |
|---|---|---|
| Biome `meadow-pipit.ogg` | Aubrey John Williams / British Library Board, [Meadow Pipit](https://commons.wikimedia.org/wiki/File:Meadow_Pipit_(Anthus_pratensis)_(W1CDR0001526_BD4).ogg) | [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/) |
| Biome `alpine-chiffchaff.ogg` | Diana Doyle, [Common Chiffchaff XC501590](https://commons.wikimedia.org/wiki/File:Phylloscopus_collybita_-_Common_Chiffchaff_XC501590.mp3) | [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/) |
| Biome `temperate-forest.ogg` | nille, [PDSounds woodland ambience](https://commons.wikimedia.org/wiki/File:20090610_0_ambience.ogg) | [Public domain dedication](https://www.pdsounds.org/help) |
| Biome `pine-wind.ogg` | W.carter, [Swedish pine forest wind](https://commons.wikimedia.org/wiki/File:Wind_in_Swedish_pine_forest_at_25_mps.ogg) | [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/) |
| Biome `dry-grass-wind.ogg` | felix.blume, [dry grass in Chile](https://freesound.org/people/felix.blume/sounds/146436/) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| Biome `ocean-waves.ogg` | tec_studio, [Ocean waves](https://freesound.org/people/tec_studio/sounds/232186/) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| Biome `beach-waves.ogg` | 3bagbrew, [Beach waves](https://freesound.org/people/3bagbrew/sounds/59564/) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| Biome `riparian-stream.ogg` | kentspublicdomain, [River, stream and creek](https://freesound.org/people/kentspublicdomain/sounds/325182/) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| Biome `swamp-ambience.ogg` | RyanKingArt, [Swamp ambience](https://freesound.org/people/RyanKingArt/sounds/547690/) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| Biome alternate `swamp-camargue-sunset.ogg` | YannSauvin, [Camargue swamp at sunset](https://freesound.org/people/YannSauvin/sounds/781709/) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| Biome water alternate `lake-ontario-waves.ogg` | Dsw4, [Lake Ontario waves](https://commons.wikimedia.org/wiki/File:Waves.ogg) | Public domain dedication by uploader |
| Foley `gravel-footsteps.ogg` | Secretlondon, [Walking on gravel](https://commons.wikimedia.org/wiki/File:Walkingongravel.ogg) | [CC BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0/) |
| Weather `light-rain.ogg` | Andron827, [Light rain](https://freesound.org/people/Andron827/sounds/142936/) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| Weather `street-rain.ogg` | BonnyOrbit, [Light rain on street](https://freesound.org/people/BonnyOrbit/sounds/380651/) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| Weather `heavy-rain.ogg` | KaleidacousticsAudio, [Heavy rain](https://freesound.org/people/KaleidacousticsAudio/sounds/630424/) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| Weather `rain-on-leaves.ogg` | SamsterBirdies, [Rain on leaves](https://freesound.org/people/SamsterBirdies/sounds/584272/) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| Weather `distant-thunder.ogg` | richwise, [Distant thunder](https://freesound.org/people/richwise/sounds/361401/) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| Weather `thunder-clap.ogg` | Fission9, [Thunder](https://freesound.org/people/Fission9/sounds/465314/) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| Weather `thunderstorm-bed.ogg` | RHumphries, [Thunderstorm bed](https://commons.wikimedia.org/wiki/File:Rhumphries_-_rbh-thunder-storm.ogg) | [CC BY 3.0](https://creativecommons.org/licenses/by/3.0/), attribution required |

The recordings have not been auditioned as a set and no claim is made about noise floor, loop points, or audible quality. The original forest, meadow, pine and lake files are preserved in the archive; the shipped forest Ogg contains a data stream as well as its Vorbis audio stream, so its runtime copy was explicitly transcoded from the audio stream. The accepted thunderstorm weather file is similarly converted for the future catalogue.

The research manifests were created before the last accepted downloads and are historical. The authoritative accepted-file list is the download audit and this report. The still-absent gentle wind, strong wind, and synthetic indoor snowstorm were rejected for quality and were not fetched. `SnowFall.ogg` was rejected by the user as music and must remain excluded.

## Playtest

Use the 1024-cell gallery (256 m world; 54 m biome pads) to visit each biome:

```powershell
cargo xtask play --worldgen gallery --worldgen-size 1024 --release
```

Press `F` to toggle the free camera; use the mouse to look, `W/A/S/D` to move, `Space` / `Ctrl` to rise and descend, and `Shift` to move faster. The listener uses this camera position during flight. Close with the window close button. Check logs for `biome ambience selection changed`; missing audio output or an unreadable file logs a warning and does not stop the session.

`examples/sandbox/assets/audio/` is used for Cargo runs independent of the working directory. The portable package copies this directory next to the executables, which supports editor-launched play. Weather is present in the package catalogue but is not automatically mixed into biome ambience. The audio-device playtest is hardware-dependent and remains a manual check.

## Validation

- `cargo test -p spall_client audio::tests`: 3 passed, covering fades, same-track reuse, and climate-field guards.
- `cargo test -p sandbox --features client --lib audio::tests`: 4 passed, covering biome assignments, silent Snow/Scree, decoder samples from all 19 packaged recordings, and the short Alpine file across its loop boundary.
- `cargo check -p sandbox --features client --bin sandbox-client`: passed.
- `cargo check -p sandbox --bin sandbox-server`: passed with the client feature and audio runtime absent.
- `cargo fmt --package spall_client --package sandbox -- --check`: run after source formatting.
- Hardware output and portable release packaging have not been playtested in this worktree; the coordinator will run release/package checks after integration.
