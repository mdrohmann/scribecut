"""Everything an edit needs before the plan, in one run.

Usage: python3 -I prep.py <recording dir> <work dir> [--db -42] [--min 0.3]

1. Uses <dir>/recording.transcript.json, or creates it with `scribecut --transcribe`
   (~30 s for a 5 min recording) when it is missing.
2. Measures the audio level in 50 ms frames and finds pauses.
3. Corrects the word times: Whisper stretches words across pauses ("next" 274.89-280.85
   when it was said at 280.8), so each word is trimmed to its longest stretch of speech.
4. Writes to <work dir>:
     levels.txt      one dB value per 50 ms frame
     silences.txt    "start end" per pause (input for make_cuts.py)
     words.json      words with corrected times; "unsure": true when the word lies
                     entirely in a pause (its time is a guess)
     overview.txt    one sentence per line with its time, then likely retakes
   and prints overview.txt.
"""
import array
import json
import math
import os
import re
import subprocess
import sys

args = sys.argv[1:]
opts = {"--db": "-42", "--min": "0.3"}
pos = []
i = 0
while i < len(args):
    if args[i] in opts:
        opts[args[i]] = args[i + 1]
        i += 2
    else:
        pos.append(args[i])
        i += 1
if len(pos) != 2:
    sys.exit(__doc__)
rec, work = pos
os.makedirs(work, exist_ok=True)
media = os.path.join(rec, "recording.mkv")
tjson = os.path.join(rec, "recording.transcript.json")
FRAME = 0.05

# 1. transcript
if not (os.path.exists(tjson) and os.path.getsize(tjson) > 0):
    print("No transcript yet - running scribecut --transcribe", file=sys.stderr, flush=True)
    r = subprocess.run([os.path.expanduser("~/projects/scribecut/target/release/scribecut"),
                        media, "--transcribe"], capture_output=True, text=True)
    if r.returncode:
        sys.exit("Transcription failed: " + r.stderr[-500:])
words = json.load(open(tjson))

# 2. levels and pauses
raw = subprocess.run(
    ["ffmpeg", "-nostdin", "-v", "quiet", "-i", media, "-map", "0:a", "-af", "highpass=f=100",
     "-ac", "1", "-ar", "8000", "-f", "s16le", "-"], capture_output=True, check=True).stdout
d = array.array("h", raw)
W = int(8000 * FRAME)
levels = []
for k in range(0, len(d), W):
    c = d[k:k + W]
    r = math.sqrt(sum(x * x for x in c) / max(1, len(c))) + 1e-9
    levels.append(20 * math.log10(r / 32768))
thr, mind = float(opts["--db"]), float(opts["--min"])
sil = []
start = None
for k, lv in enumerate(levels + [0.0]):
    q = lv < thr
    if q and start is None:
        start = k
    if not q and start is not None:
        if (k - start) * FRAME >= mind - 1e-9:
            sil.append((round(start * FRAME, 2), round(k * FRAME, 2)))
        start = None
with open(os.path.join(work, "levels.txt"), "w") as f:
    f.write("\n".join(f"{v:.1f}" for v in levels) + "\n")
with open(os.path.join(work, "silences.txt"), "w") as f:
    f.write("".join(f"{a:.2f} {b:.2f}\n" for a, b in sil))

# 3. corrected word times
out = []
for w in words:
    s, e = w["start"], w["end"]
    parts = [(s, e)]
    for a, b in sil:
        if b <= s or a >= e:
            continue
        nxt = []
        for p, q in parts:
            if a > p:
                nxt.append((p, min(a, q)))
            if b < q:
                nxt.append((max(b, p), q))
        parts = [(p, q) for p, q in nxt if q - p > 1e-6]
    item = {"text": w["text"].strip(), "start": round(s, 2), "end": round(e, 2)}
    if parts:
        p, q = max(parts, key=lambda x: x[1] - x[0])
        item["start"], item["end"] = round(p, 2), round(q, 2)
    else:
        item["unsure"] = True
    out.append(item)
json.dump(out, open(os.path.join(work, "words.json"), "w"), indent=0)


# 4. overview
def t(x):
    m, s = divmod(x, 60)
    return f"{int(m)}:{s:04.1f}"


def norm(s):
    return re.sub(r"[^a-z0-9']", "", s.lower())


lines = []
cur = []
first = 0
for k, w in enumerate(out):
    if cur:
        gap = w["start"] - out[k - 1]["end"]
        if gap >= 1.0:
            cur.append(f"…{gap:.0f}s…")
    else:
        first = k
    cur.append(w["text"])
    if k == len(out) - 1 or re.search(r"[.?!]$", w["text"]):
        lines.append(f"{t(out[first]['start'])}  {' '.join(cur)}")
        cur = []

STOP = set("a an the and or but so to of in on at for is it this that we i you be are was "
           "with from by as can our they there what how".split())
toks = [norm(w["text"]) for w in out]
reps = []
for i in range(len(toks)):
    for j in range(i + 1, len(toks)):
        if out[j]["start"] - out[i]["start"] > 150:
            break
        if i > 0 and toks[i - 1] == toks[j - 1] and j - 1 != i:
            continue  # not maximal: the match starts earlier
        n = 0
        while j + n < len(toks) and i + n < j and toks[i + n] == toks[j + n]:
            n += 1
        if n >= 3 and any(x not in STOP for x in toks[i:i + n]):
            reps.append((i, j, n))
reps.sort(key=lambda r: (-r[2], r[0]))
seen = set()
rep_lines = []
for i, j, n in reps:
    key = (i, j)
    if any((i + a, j + a) in seen for a in range(-n, n)):
        continue
    seen.add(key)
    phrase = " ".join(out[x]["text"] for x in range(j, j + n))
    rep_lines.append(f"{t(out[i]['start'])} and {t(out[j]['start'])}  ({n} words) {phrase}")
rep_lines = sorted(rep_lines[:30], key=lambda s: [float(p) for p in s.split()[0].split(":")])

text = ("# Sentences (start time, corrected; …Ns… = pause of N seconds)\n" + "\n".join(lines)
        + "\n\n# Repeated phrases (likely retakes: keep the last complete take)\n"
        + ("\n".join(rep_lines) or "(none)") + "\n")
text += (f"\n# {len(sil)} pauses; duration {t(len(levels) * FRAME)}; "
         f"{sum(1 for w in out if w.get('unsure'))} words with guessed times\n")
open(os.path.join(work, "overview.txt"), "w").write(text)
print(text, end="")
