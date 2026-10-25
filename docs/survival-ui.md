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
to the server or decides an outcome. Cold, wetness and injury come from the server's
authoritative `PlayerHealth` record (ENG-223, ENG-264/265).

| State | Driven by | On screen |
| --- | --- | --- |
| Cold | Lost warmth from the server (`spall_sim::warmth`): air temperature and time of day, wind chill in the open, shelter, wetness and nearby fires | Edge frost creeps in, colour drains and cools toward blue |
| Wet | Wetness from the server: water depth over the feet (1.5 m soaks), rain without a roof (snow counts 10%); drying takes a few minutes, faster when warm or by a fire | Slight dark, cool vignette and tint |
| Injury *(preview)* | Admin menu only until ENG-223 health lands | Colour drains, dark-red vignette with a slow, soft heartbeat (48 to 78 bpm as injury rises), slight edge blur |
| Thirst *(preview)* | Admin menu only | Edge blur, slight desaturation |
| Exhaustion *(preview)* | Admin menu only | Dark vignette, slight blur |
| Hunger *(preview)* | Admin menu only | Mild desaturation only; hunger is mainly sound and hand tremble |

To see the effects in a game, open the admin menu (F10). The **Body** section shows
live cold/wet and has steppers for the preview looks. Set the season to winter and force a
Blizzard, then stand in the open to see frost build over about a minute; swim to get soaked.

**Reminder icons:** any state at 70% or more also shows a tinted, hand-drawn icon in
the lower-right corner. It fades in between 70% and 80%; the most urgent state sits
rightmost. The icons are `crates/spall_client/assets/hud/*.png`, drawn by
`make_icons.py` in the same folder (needs Pillow).

Cold and wet are server-authoritative now (ENG-264/265); thirst, exhaustion and hunger will replace
their preview inputs the same way, and the grade mapping stays.

## Built: the pack (Tab)

`Tab` slides a weathered canvas tarp up over the screen, dims the world a little,
and lays the inventory out on it. It is UI only: nothing is drawn in the world,
the camera does not move, and the world keeps running behind it. Each stack shows
the item's painted icon, a charcoal count ("×12") and its name; the stack under
the cursor grows slightly and its name darkens. Text scales with the tarp. `Tab` or
`Esc` packs it away again, and walking or jumping closes it. Clicking a tool (the
shovel or the axe) puts it in hand, marked "(in hand)"; clicking it again puts it away. Left
click swings the tool in hand; empty-handed, it does nothing.

- **Inventory.** The server keeps the inventory as stable item IDs. While the pack
  is open the client asks for it (`ProgressionRequest::InspectInventory`) every
  1.5 s, so harvests show up without reopening.
- **Engine and game.** The engine draws the tarp
  (`crates/spall_client/assets/hud/tarp.png`, drawn by `make_tarp.py`), lays out up
  to 12 stacks (4 by 3; more show a "+N more" note) and loads icons on first sight.
  The game's `spall_client::window::PackView` gives each item ID a name and PNG
  icon. The sandbox's icons are `examples/sandbox/assets/items/*.png`, drawn by
  `make_items.py`.
- **Not yet:** crafting on the cloth, moving items, belt slots, and sounds.

## Built: the blueprint book (B)

`B` slides an open leather journal over the screen. Its pages hold a blue
blueprint card for each of the game's recipes: what it makes, a small drawing of
where its materials go, and each material as have / need (red when short).
Clicking a card turns to the crafting cloth: the blueprint on the left page, a
canvas cloth on the right, and a strip of the pack's materials under it.

Blueprints are not a grid. Each lists **slots**: a position on the cloth, the item
it takes and the item's drawn size, and the blueprint is seen either **from above**
or **from the side** (side views draw a ground line). Empty slots show as faint
outlines of their item. Drag a material onto the cloth and it snaps into the
nearest empty slot that takes it (within the slot's half size plus 8% of the
cloth); the slot it would snap into is lit while dragging, and a drop with no home
goes back to the pack. Drag a laid material to move it, or click it to take it
back. When every slot is filled, **Craft** lights up and asks the server for one
batch (`ProgressionOperation::Craft`). On success the cloth clears and says what
was made; a refusal keeps the cloth and says why. A craft beaten by a harvest
(stale inventory revision) is retried once. `B` or `Esc` closes the book, `Tab`
swaps to the pack, and walking closes it.

The sandbox's blueprints:

| Blueprint | View | Lay out | Makes |
| --- | --- | --- | --- |
| Planks | Side | a log on the ground | 4 planks |
| Stone block | Side | two stones stacked on two | 1 stone block |
| Sticks | Above | a log | 6 sticks |
| Fire pit | Above | six sticks in the centre, six stones in a ring round them | 1 fire pit |

Sticks and the fire pit are new items (recipe catalogue version 2). Clicking a
fire pit in the open pack sets it down, lit, on open ground in front of the
player (ENG-266); E beside it feeds it a log or a stick from the pack. See
[architecture](architecture.md) for fires.

- **Engine and game.** The engine draws the book and cloth
  (`crates/spall_client/assets/hud/book.png` and `cloth.png`, drawn by
  `make_book.py`), lays out up to 12 blueprints, snaps materials and sends the
  craft. The game's `PackView::blueprints` gives each blueprint's recipe, output,
  view and slots; the sandbox checks each against its server recipe.
- **Shared inventory.** The pack and the book read one `InventoryFeed`, refreshed
  every 1.5 s while either is open.
- **Not yet:** more than 12 blueprints (paging), rotated items on the cloth,
  blueprints learned in play, sounds.

## Planned

- **Sounds** for the body states and inventory (list below). None is integrated yet.
- **Belt:** 4 to 6 worn quick slots, visible on the player body, switched without a menu.
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
| Pack open / close | Tarp slides in and away | [Fission9 inventory open](https://freesound.org/people/Fission9/sounds/568992/), [Fission9 inventory close](https://freesound.org/people/Fission9/sounds/568991/) |
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
