"""Write [[cut]] and [[slide]] entries into a scribecut edit file from a plan.

Usage: python3 -I make_cuts.py <plan.json> <silences.txt> <recording.edit.toml>

The first run copies the edit file to <edit file>.orig; every run rebuilds from that
copy, so the script can be re-run after changing the plan. Prints the resulting length
and every non-pause cut and slide with its time in the EDITED video.

plan.json (times on the ORIGINAL recording's timeline):
{
  "keep":   [["^Okay, this is", "Jack.$", "intro + goals"], [32.9, 62.7, "demo"], ...],
  "notes":  {"6.4-32.9": "repeated: first two takes of the goal"},   # optional, for gaps between keeps
  "extra_cuts": [["^to the shared view@0:39", "^to the shared view@0:41", "repeated phrase"]],
  "slides": [{"from": 0, "to": "^So this is the view", "file": "slides.pdf", "page": 1},
             {"from": "^So the next steps@4:34", "to": "end", "file": "slides.pdf", "page": 2}],
  "pad": 0.10,          # optional: silence left on each side of a pause cut
  "min_pause": 0.35     # optional: shorter pauses are left alone
}
A time is seconds (32.9), "m:ss.ss", or a phrase anchor:
  "^words"   the boundary just BEFORE the words (start of a keep, start of a cut)
  "words$"   the boundary just AFTER them (end of a keep)
  "...@m:ss" picks the occurrence starting nearest that time (needed when the words repeat)
Matching ignores case and punctuation. Anchors use the recording's transcript.json and levels.txt from prep.py
(next to silences.txt). An anchor lands in the middle of the pause at that word boundary;
with no pause there it takes the quietest 50 ms nearby and prints a warning to listen to.
A keep may carry a 4th element, the note for the gap BEFORE it ("why this was cut");
with anchors that is easier than "notes", whose keys are resolved times.
Everything outside the keep ranges is cut. Inside them, pauses from silences.txt are cut.
"""
import json
import os
import re
import shutil
import sys

plan_path, sil_path, edit_path = sys.argv[1:4]
plan = json.load(open(plan_path))
orig = edit_path + ".orig"
if not os.path.exists(orig):
    shutil.copy(edit_path, orig)
base_text = open(orig).read()
work = os.path.dirname(os.path.abspath(sil_path))
FRAME = 0.05
warnings = []


def secs_plain(v):
    parts = [float(p) for p in str(v).split(":")]
    s = 0.0
    for p in parts:
        s = s * 60 + p
    return s


def t(x):
    m, s = divmod(round(x, 2), 60)
    return f"{int(m)}:{s:05.2f}"


m = re.search(r'^duration\s*=\s*"([^"]+)"', base_text, re.M)
DUR = secs_plain(m.group(1))
PAD = plan.get("pad", 0.10)
MIN_PAUSE = plan.get("min_pause", 0.35)
sil = [tuple(map(float, l.split())) for l in open(sil_path) if l.strip()]

words = levels = None


def load_words():
    global words, levels
    if words is None:
        tj = re.sub(r"\.edit\.toml$", "", edit_path) + ".transcript.json"
        words = json.load(open(tj))
        try:
            levels = [float(l) for l in open(os.path.join(work, "levels.txt")) if l.strip()]
        except FileNotFoundError:
            sys.exit(f"Phrase anchors need levels.txt in {work} - run prep.py")
    return words


def norm(s):
    return re.sub(r"[^a-z0-9']", "", s.lower())


def quietest(lo, hi):
    ks = range(max(0, int(lo / FRAME)), min(len(levels), int(hi / FRAME) + 1))
    k = min(ks, key=lambda k: levels[k])
    return k * FRAME + FRAME / 2, levels[k]


def anchor(spec):
    before = spec.startswith("^")
    body = spec[1:] if before else spec
    if not before and not body.endswith("$"):
        sys.exit(f"Anchor {spec!r}: start it with ^ (before the words) or end it with $ (after)")
    body = body.rstrip("$")
    near = None
    if "@" in body:
        body, near = body.rsplit("@", 1)
        near = secs_plain(near)
    want = [norm(x) for x in body.split() if norm(x)]
    ws = load_words()
    toks = [norm(w["text"]) for w in ws]
    hits = [i for i in range(len(toks) - len(want) + 1) if toks[i:i + len(want)] == want]
    if not hits:
        sys.exit(f"Anchor {spec!r}: words not found in the transcript")
    if len(hits) > 1 and near is None:
        sys.exit(f"Anchor {spec!r} occurs {len(hits)} times, at "
                 + ", ".join(t(ws[i]["start"]) for i in hits) + " - add @m:ss")
    i = min(hits, key=lambda i: abs(ws[i]["start"] - near)) if near is not None else hits[0]
    # Whisper's word times drift by up to a second around pauses, so look for the pause
    # in a window that leans away from the words: before them for ^, after them for $.
    if before:
        edge = ws[i]["start"]
        cands = [(a, b) for a, b in sil if a <= edge + 0.1 and b >= edge - 1.5]
        pick = max(cands, key=lambda s: s[0]) if cands else None
    else:
        edge = ws[i + len(want) - 1]["end"]
        cands = [(a, b) for a, b in sil if b >= edge - 0.1 and a <= edge + 1.5]
        pick = min(cands, key=lambda s: s[1]) if cands else None
    if pick:
        return round((pick[0] + pick[1]) / 2, 2)
    x, db = quietest(edge - 0.25, edge + 0.25)
    warnings.append(f"{spec!r}: no pause at {t(edge)}, cut at the quietest point {t(x)} "
                    f"({db:.0f} dB) - listen to it")
    return round(x, 2)


resolved = {}


def secs(v):
    if isinstance(v, (int, float)):
        return float(v)
    if v.startswith("^") or v.endswith("$"):
        if v not in resolved:
            resolved[v] = anchor(v)
        return resolved[v]
    return secs_plain(v)


keep = sorted((secs(k[0]), secs(k[1]), k[2] if len(k) > 2 else "") for k in plan["keep"])
notes = {}
for k, v in plan.get("notes", {}).items():
    a, b = k.split("-")
    notes[(round(float(a), 2), round(float(b), 2))] = v
# Notes may also sit on the keep that FOLLOWS a gap, as "note_before" (works with anchors).
for k in plan["keep"]:
    if len(k) > 3:
        notes[("before", round(secs(k[0]), 2))] = k[3]

cuts = []
if keep[0][0] > 0:
    cuts.append((0.0, keep[0][0], "pause"))
for (a, b, _), (c, d, _) in zip(keep, keep[1:]):
    if c > b:
        cuts.append((b, c, notes.get((round(b, 2), round(c, 2)))
                     or notes.get(("before", round(c, 2)), "removed")))
if keep[-1][1] < DUR:
    cuts.append((keep[-1][1], DUR, "removed"))
for a, b, _ in keep:
    for s, e in sil:
        s, e = max(s, a), min(e, b)
        if e - s >= MIN_PAUSE and s + PAD < e - PAD:
            lo = s + PAD if s > a else s
            hi = e - PAD if e < b else e
            cuts.append((lo, hi, "pause"))
for a, b, n in plan.get("extra_cuts", []):
    cuts.append((secs(a), secs(b), n))
cuts.sort()
merged = []
for c in cuts:
    if merged and c[0] <= merged[-1][1] + 0.05:
        p = merged[-1]
        note = p[2] if p[2] != "pause" else c[2]
        merged[-1] = (p[0], max(p[1], c[1]), note)
    else:
        merged.append(c)
removed = sum(b - a for a, b, _ in merged)


def edited(x):
    return x - sum(min(b, x) - a for a, b, _ in merged if a < x)


slides = []
for s in plan.get("slides", []):
    to = DUR if s["to"] == "end" else secs(s["to"])
    slides.append((secs(s["from"]), to, s["file"], int(s.get("page", 1))))

out = [f'[[cut]]\nfrom = "{t(a)}"\nto = "{t(b)}"\nnote = {json.dumps(n)}\n' for a, b, n in merged]
for a, b, f, p in slides:
    out.append(f'[[slide]]\nfrom = "{t(a)}"\nto = "{t(b)}"\nfile = {json.dumps(f)}\npage = {p}\n')
head = base_text.split("\n[[")[0].rstrip() + "\n"
open(edit_path, "w").write(head + "\n" + "\n".join(out))

print(f"{len(merged)} cuts, output {DUR - removed:.1f}s ({t(DUR - removed)})")
print("\n  edited   original           note")
for a, b, n in merged:
    if n != "pause":
        print(f"  {t(edited(a)):>7}  {t(a)}-{t(b)}  {n}")
for a, b, f, p in slides:
    print(f"  {t(edited(a)):>7}  slide {f} p{p} until {t(edited(b))} (original {t(a)}-{t(b)})")
for w in warnings:
    print("WARNING " + w)
