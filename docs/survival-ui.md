# Survival UI

Direction chosen 2026-10-21 (ENG-229): **"Tarp and Twine"**, a handmade survivalist
look where the body carries most information, with a hybrid **belt / laid-out pack /
camp** inventory. Information uses the first layer that can carry it:

1. **The world:** smoke, tracks, chipped voxels on a worn tool, food changing colour.
2. **The body:** breathing, shivering, limping, and the screen grade below.
3. **Something held:** a knotted wrist cord (days), a charcoal journal (map, plans).
4. **A thin overlay:** only what nothing else can show (interaction prompts, item names).

## Built: screen grade

`spall_render::ScreenGrade` is a generic full-screen grade applied in the tone-map
pass after tone mapping: saturation, colour tint, vignette (colour and strength),
edge blur and edge frost. `ScreenGrade::IDENTITY` leaves the image unchanged, and
debug views (F4) ignore it. `GameRenderer::set_screen_grade` sets it per frame.

`spall_client::body_state` drives it. This is **presentation only**: nothing is sent
to the server or decides an outcome.

| State | Driven by | On screen |
| --- | --- | --- |
| Cold | Replicated air temperature, minus wind chill in the open and chill from being wet | Edge frost creeps in, colour drains and cools toward blue |
| Wet | Rain or storm at the player, scaled by open sky (snow counts 30%); waist-deep water soaks within about a second; drying takes a few minutes, faster when warm | Slight dark, cool vignette and tint |
| Injury *(preview)* | Admin menu only until ENG-223 health lands | Colour drains, dark-red vignette with a slow, soft heartbeat (48 to 78 bpm as injury rises), slight edge blur |
| Thirst *(preview)* | Admin menu only | Edge blur, slight desaturation |
| Exhaustion *(preview)* | Admin menu only | Dark vignette, slight blur |
| Hunger *(preview)* | Admin menu only | Mild desaturation only; hunger is mainly sound and hand tremble |

To see the effects in a game, open the admin menu (F10). The **Body** section shows
live cold/wet and has steppers for the preview looks. Force Snow or Blizzard weather
to see frost build over about a minute, or swim to get soaked.

**Reminder icons:** any state at 70% or more also shows a tinted, hand-drawn icon in
the lower-right corner. It fades in between 70% and 80%; the most urgent state sits
rightmost. The icons are `crates/spall_client/assets/hud/*.png`, drawn by
`make_icons.py` in the same folder (needs Pillow).

When survival stats become server-authoritative, they replace these inputs; the
grade mapping stays.

## Built: the pack (Tab)

`Tab` kneels and unrolls a canvas tarp on the ground in front of the player,
weighted with stones at the corners, and lays the inventory out on it. Each stack
shows up to four copies of the item's model with a small label ("Log ×12") over
it; the stack under the cursor lifts and its label brightens. `Tab` or `Esc` rolls
it back up, and walking or jumping gets up and goes. The world keeps running.

- **Client-side only.** The tarp and items are overlay cubes, like the animals; the
  server keeps the inventory as stable item IDs. While the pack is open the client
  asks for it (`ProgressionRequest::InspectInventory`) every 1.5 s, so harvests show
  up without reopening.
- **Game-owned look.** `spall_client::window::PackView` gives names and cube models
  per item ID, and the tarp, twine and stone materials. The sandbox's
  `SandboxPackView` draws logs, planks, stone chunks and stone blocks from manifest
  materials. Up to 12 stacks fit (4 by 3); more show a "+N more" note.
- **Not yet:** crafting on the cloth, moving items, belt slots, sounds, and a tarp
  that follows uneven ground (it lies flat at the player's feet height).

## Planned

- **Sounds** for the body states and inventory (list below). None is integrated yet.
- **Belt:** 4 to 6 worn quick slots, visible on the player body, switched without a menu.
- **Pack crafting:** craft on the cloth from the laid-out items; other players see
  you kneeling.
- **Camp:** being rethought; a first cut (a claimed area around a fire pit) was tried and taken out.
- **Wrist cord and journal:** the day count and exact readouts for players who want
  numbers.

## Sound list (candidates, not yet auditioned or integrated)

All links are Freesound searches filtered to **CC0**, or specific CC0 results from
those searches. Accepted files go into `examples/sandbox/assets/audio/` and
`CREDITS.md` the same way as the biome audio.

### Body state

| Cue | Use | Candidates |
| --- | --- | --- |
| Heartbeat | Injury; speeds up with damage | [daandraait slow heartbeat](https://freesound.org/people/daandraait/sounds/249716/), [Cloud-10 97 BPM](https://freesound.org/people/Cloud-10/sounds/688735/), [loudernoises 80 BPM](https://freesound.org/people/loudernoises/sounds/332819/), [search](https://freesound.org/search/?q=heartbeat&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Heavy breathing | Exhaustion, sprint recovery | [darranambler](https://freesound.org/people/darranambler/sounds/321669/), [sherby168 huff and puff](https://freesound.org/people/sherby168/sounds/542078/), [search](https://freesound.org/search/?q=heavy+breathing+exhausted&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Cold exhale | Cold; pairs with visible breath | [LilMati frosty exhale](https://freesound.org/people/LilMati/sounds/519438/), [search](https://freesound.org/search/?q=cold+breath&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Shiver | Cold above 50% | [stuniverso trembling cold](https://freesound.org/people/stuniverso/sounds/761567/), [Alivvie shiver](https://freesound.org/people/Alivvie/sounds/436785/) |
| Teeth chattering | Cold above 80% | [aisselles](https://freesound.org/people/aisselles/sounds/706219/), [Vanalosswen](https://freesound.org/people/Vanalosswen/sounds/641217/) |
| Stomach growl | Hunger | [R1nkata gurgle](https://freesound.org/people/R1nkata/sounds/720820/), [SamanthaCastleberry long growl](https://freesound.org/people/SamanthaCastleberry/sounds/696405/), [shaunlolz](https://freesound.org/people/shaunlolz/sounds/328651/) |
| Dry cough | Thirst, smoke | [OwlStorm](https://freesound.org/people/OwlStorm/sounds/151217/), [qubodup double cough](https://freesound.org/people/qubodup/sounds/743360/) |
| Gulp | Drinking | [magnuswaker hard swallow](https://freesound.org/people/magnuswaker/sounds/531755/), [Christopherderp drinking](https://freesound.org/people/Christopherderp/sounds/342233/) |
| Chewing | Eating | [search](https://freesound.org/search/?q=eating+chewing&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Pain grunt | Taking damage | [elynch0901 male grunting](https://freesound.org/people/elynch0901/sounds/464486/), [MrFossy pain grunts](https://freesound.org/people/MrFossy/sounds/547209/) |
| Relief exhale | Warming by a fire, drinking when parched | [Sadiquecat relief](https://freesound.org/people/Sadiquecat/sounds/801137/), [esperar phews](https://freesound.org/people/esperar/sounds/171943/) |
| Ear ringing | Heavy hit or nearby blast | [Ear ringing long](https://freesound.org/people/deleted_user_9482149/sounds/456698/), [Sclolex tinnitus](https://freesound.org/people/Sclolex/sounds/210533/) |
| Underwater muffle | Head under water (bed) | [GirlWithSoundRecorder](https://freesound.org/people/GirlWithSoundRecorder/sounds/734743/), [Osiruswaltz loopable](https://freesound.org/people/Osiruswaltz/sounds/530167/) |
| Wet clothes | Moving while soaked | [shutuplaika wet clothing peel](https://freesound.org/people/shutuplaika/sounds/651106/), [search](https://freesound.org/search/?q=wet+cloth&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Dripping | Leaving water | [magnuswaker water drip](https://freesound.org/people/magnuswaker/sounds/524223/), [search](https://freesound.org/search/?q=drip&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |

### Inventory and held items

| Cue | Use | Candidates |
| --- | --- | --- |
| Tarp unroll | Opening the pack | [craigsmith tent/tarp whoosh](https://freesound.org/people/craigsmith/sounds/481896/), [search](https://freesound.org/search/?q=tarp&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Pack open / close | Kneel, roll the tarp back up | [Fission9 inventory open](https://freesound.org/people/Fission9/sounds/568992/), [Fission9 inventory close](https://freesound.org/people/Fission9/sounds/568991/) |
| Fabric handling | Moving items on the tarp | [dynamique fabric rubbing](https://freesound.org/people/dynamique/sounds/554561/) |
| Pouch | Belt slot grab | [Canakinsound coin pouch](https://freesound.org/people/Canakinsound/sounds/722039/) |
| Wood stick | Picking up sticks or logs | [gashasound wood stick](https://freesound.org/people/gashasound/sounds/470914/), [search](https://freesound.org/search/?q=stick+wood&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Rope / twine | Crafting confirm, wrist cord | [stujun rope](https://freesound.org/people/stujun/sounds/351618/), [search](https://freesound.org/search/?q=rope&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Journal page | Open journal, turn page | [mmaruska page turn](https://freesound.org/people/mmaruska/sounds/177867/), [LilMati page turn](https://freesound.org/people/LilMati/sounds/397550/) |
| Charcoal sketch | Map filling in | [GeorgeHopkins pencil lines](https://freesound.org/people/GeorgeHopkins/sounds/650994/), [Yuval drawing on paper](https://freesound.org/people/Yuval/sounds/185601/) |

### World feedback

| Cue | Use | Candidates |
| --- | --- | --- |
| Campfire close | Warmth source; drives cold relief | [Ambient-X adding kindling](https://freesound.org/people/Ambient-X/sounds/660298/), [search](https://freesound.org/search/?q=campfire&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Snow footsteps | Walking in snow | [Nimlos](https://freesound.org/people/Nimlos/sounds/432285/), [cabled_mess](https://freesound.org/people/cabled_mess/sounds/384422/) |
| Rain on tarp | Shelter cue (dry under cover) | [freezound5](https://freesound.org/people/freezound5/sounds/141440/), [JDTTL](https://freesound.org/people/JDTTL/sounds/843503/) |
| Bone crack | Fall injury | [Kane53126](https://freesound.org/people/Kane53126/sounds/257927/) |
