# The controller UI, a mock

One self-contained page, `controller.html`: a clickable mock of what dform's controller UI could be, built 2026-10-15 to bootstrap the design, not to commit to. Open it in a browser; its design note and critique are the comment at the top of the file.

The frame it settled on: the primary screen is the estate (entities, relationships, health and the four-source reading on each node), provider instances as regions, deployments as lanes; the plan is a propagation overlay; no edges at rest, position is dependency; progressive disclosure (a card is its state, the reasons are the hover's `why`); the controller's clock is events and runs, ticks only inside a run; the levers are the controller's real ones (sign, approve a drift revert); no sync button, no red-amber-green. It is not Argo's object tree ("where does it live") but "what depends on what, what is wrong, where would a change go".

The data is the three estates under examples/, terry.org's release system, and the user's own project; digests, times, run numbers and call counts are invented.
