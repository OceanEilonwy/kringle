# Kringle design canvas

Source for the design canvas at https://claude.ai/artifact/FhPPdFgyqk2oxUHRRWYvZ9

`project/` holds the canvas index (`canvas.json`) and one `.dc.html` file per artboard:

- `Main` (overview) and `Themes` (the Bethlehem style sheet: palette, type, components, motifs, legibility rules)
- Screens: `Create`, `Admin` (`view`: collecting | drawn | impossible | long), `Join`, `Joined`, `Reveal` (`wrapped`, `sample`: typical | long).
  Each takes `device` (desktop | mobile) and `sky` (day, the default, or storybook | realistic | aurora | dawn).
- The other files are thin wrappers that import a screen with given props.

The app in `src/main.rs` implements the Daytime sky. Its town and cloud artwork (`static/town.svg`, `static/clouds.svg`) was exported from `Create.dc.html`.

To republish after edits, publish `project/canvas.json` plus the changed files to the artifact URL above.
