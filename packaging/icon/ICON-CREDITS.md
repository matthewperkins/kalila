# kalila icon credits

The kalila icon (`kalila-icon.svg` and the `kalila-*.png` renders) shows **Kalila and Dimna**,
the two jackals whose conversations frame the first books of the *Panchatantra*, known in its
Arabic and later European versions as *Kalila wa-Dimna* or the *Tales of Bidpai*. Two jackal
heads in profile overlap like stacked silhouettes, Kalila in vermilion and Dimna in slate blue,
on a parchment tile with a gold border, with a small flower and leaf. It is the same drawing
as the icon of its sibling program, dimna, rearranged.

It is an **original drawing**, written by hand as SVG paths and shapes. No image was traced,
embedded or vectorised. It takes general inspiration, in mood only, from:

- the illustrated Arabic manuscripts of *Kalila wa-Dimna* (13th–14th century), which paint
  Dimna blue and Kalila red on parchment with gold and dark green foliage and thin dark
  outlines;
- the cover of Ramsay Wood's retelling, *Kalila and Dimna: Selected Fables of Bidpai*, for the
  flat warm colours and the long-muzzled, tall-eared jackal profile. Nothing from that
  illustration (by Margaret Kilrenny, in copyright) is copied.

The icon was drawn by Claude Opus 5.5, an AI model made by Anthropic, at the request of the
project's owner.

**Licence:** the icon is covered by the repository's licence, GPL-3.0-or-later (see
[LICENSE](../../LICENSE)), like the code.

To re-render the PNGs: `rsvg-convert -w N -h N kalila-icon.svg -o kalila-N.png` for N in
32, 48, 64, 128, 256. `kalila-1024.png` (the macOS app's icon is made from it) was
rendered on a Mac, where `rsvg-convert` is not installed:
`swift packaging/macos/render-icon.swift kalila-icon.svg kalila-1024.png 1024`.

Until 2026-11-13 the icon showed Bastet and Sekhmet, adapted from Wikimedia Commons drawings
under CC BY-SA; those files are in the repository's history only.
