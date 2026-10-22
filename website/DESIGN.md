# sieve website: design direction

Direction supplied by the project owner: calm and technical, dark and terminal-like, friends who build apps, backend developers, and QA as the audience, primary call to action goes to the install page.

Dial: ENERGY 1 / RHYTHM 2 / MOTION 1

Reading this as: open-source developer tool site for mobile developers, backend developers, and QA, in a calm technical dark style.

## Theme

Dark is the default and the main design. Reason: sieve is a terminal tool and its own web UI is dark. A working light theme ships too, and both are verified.

## Palette

Core colors, taken from sieve's own web UI so the site and the product look like one thing:

| Role     | Dark      | Light     |
| -------- | --------- | --------- |
| Base     | `#14161a` | `#f6f7f9` |
| Panel    | `#1b1e24` | `#eceef2` |
| Text     | `#d7dce4` | `#1d2129` |
| Dim text | `#8a93a3` | `#5b6475` |

One accent: green (`#5fc47a` dark, `#16722f` light). Reason: green is what the web UI shows for a successful response, so it means "request came through". The accent is used for the logo, links, the primary button, and the focus ring. Nothing else.

## Type

IBM Plex Sans for text, IBM Plex Mono for commands and code. Reason: the two were drawn as a pair, Plex Mono has distinct letterforms that stay readable at small sizes in command lists, and Plex Sans is calm and legible without being the usual default. Mono is only for commands and code, never for headings.

## Logo

A disc with seven punched holes, like a sieve seen from above. Lowercase `sieve` next to it in the site title. Mark is green in both themes. Source: `src/assets/logo-dark.svg` and `logo-light.svg`.

## Layout

Content-driven, left-aligned. Sections change composition on purpose: hero with a real screenshot, a two-column device command list, an asymmetric "what you can do" block, a plain three-column audience list, a narrow text block, a closing call to action. No cards, no icons, no bento, no fake terminal, no stats, no testimonials, no FAQ.

## Motion

Hover and focus states only.

## Identity motif

The punched-hole disc. Used as the logo and the favicon.
