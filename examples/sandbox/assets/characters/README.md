# Sandbox character fixtures

`player.spvox` is the player body: an SPVX 1.1 articulated asset at cell-size
code `1` (6.25 cm), written by the same deterministic generator as the animals:

```powershell
cargo run -p sandbox --example animal_fixtures -- --write
cargo run -p sandbox --example animal_fixtures -- --check
```

It is not wired into the game yet: no client draws it for other players, and
nothing selects clips from movement.

## Body

28 cells tall (1.75 m), 12 cells across the arms, 5 cells deep. That fits the
1.8 m player capsule (`CharacterParams::DEFAULT`). It faces +X with +Y up; +Z is
its left. The root `pelvis` pivot is the hip joint, 13 cells above the shoe
soles. All limbs hinge about Z, and a positive angle swings a hanging limb
forward.

| Part | Id | Parent |
| --- | ---: | --- |
| pelvis (root) | 1 | — |
| torso | 2 | pelvis |
| head | 3 | torso |
| upper_arm_left / upper_arm_right | 4 / 5 | torso |
| forearm_left / forearm_right | 6 / 7 | upper arms |
| thigh_left / thigh_right | 8 / 9 | pelvis |
| shin_left / shin_right (with shoes) | 10 / 11 | thighs |

Material keys are display stand-ins: `clay` for skin, `moss` for clothes, and
`wood` for hair, belt and shoes. The palette tints carry the colours: skin,
blue shirt, dark trousers, shoes, brown hair, eyes, belt.

## Clips (60 ticks/s, all looping)

| Clip | Ticks | Motion |
| --- | ---: | --- |
| `idle` | 120 | Slow torso and arm sway |
| `walk` | 60 | ±25° leg swing, counter-swinging arms, swing-knee bend, hip dip at double support |
| `run` | 36 | ±40° legs, high knee tuck, bent elbows, 10° forward lean; meant for Shift |
| `swim` | 60 | Body rotated prone (face down, head along +X); front-crawl arms half a cycle apart; flutter kick |

Root motion is a small vertical bob only. Horizontal travel comes from the
player's movement, so clip speed should follow ground speed when this is
wired in. Preview all clips with:

```powershell
cargo run -p sandbox --features client --example animal-preview -- --bounded --out .local/animal-preview
```
