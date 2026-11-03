"""Export an edit and check the result, in one run (run it in the background: 5+ minutes).

Usage: python3 -I finish.py <recording.edit.toml> <out.mp4> [--limit m:ss] [--frames <dir>]
                             [--verify-only]   (skip the export, check an existing <out.mp4>)

1. `scribecut --check` for the expected length (stops if it fails or exceeds --limit).
2. `scribecut --export`. If this script is killed, also kill the ffmpeg it leaves behind:
   pgrep -af 'ffmpeg -nostdin'.
3. ffprobe: H.264 yuv420p + AAC in MP4 (what Slack plays) and the duration vs --check.
4. One frame from the middle of each slide range into --frames (default: next to the
   output, in <out>.frames/), to look at with Read.
5. Transcribes the export (`scribecut <out.mp4> --transcribe`, ~30 s) and prints the words
   around every non-pause cut with "‖" at the cut, so a clipped or doubled word shows up
   in text. A cut that eats a short word ("are") still needs reading closely: Whisper may
   fill it in from context.
Prints PASS/FAIL per check and exits 1 if any failed.
"""
import json
import os
import re
import subprocess
import sys

SC = os.path.expanduser("~/projects/scribecut/target/release/scribecut")
args = sys.argv[1:]
opts = {"--limit": None, "--frames": None}
verify_only = "--verify-only" in args
args = [a for a in args if a != "--verify-only"]
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
edit, out = (os.path.abspath(p) for p in pos)


def secs(v):
    if isinstance(v, (int, float)):
        return float(v)
    s = 0.0
    for p in str(v).split(":"):
        s = s * 60 + float(p)
    return s


def t(x):
    m, s = divmod(round(x, 2), 60)
    return f"{int(m)}:{s:05.2f}"


def read_edit(path):
    """An edit file's tables as nested dicts: `[source]`, `[[cut]]`, `[[slide]]`…

    Hand-written rather than `tomllib`, which arrived in Python 3.11 while macOS
    still ships 3.9 as `python3`. The edit file only ever holds flat tables of
    strings, numbers, booleans and arrays of those.
    """
    def value(raw):
        raw = raw.strip()
        if raw.startswith("["):
            inner = raw[1:-1].strip()
            return [value(x) for x in split_commas(inner)] if inner else []
        if raw[:1] in "\"'":
            return raw[1:-1]
        if raw in ("true", "false"):
            return raw == "true"
        try:
            return int(raw)
        except ValueError:
            pass
        try:
            return float(raw)
        except ValueError:
            return raw

    doc = {}
    table = doc
    lines = iter(open(path).read().splitlines())
    for line in lines:
        line = uncommented(line)
        if not line:
            continue
        if line.startswith("[["):
            name = line[2:line.index("]]")].strip()
            table = {}
            doc.setdefault(name, []).append(table)
        elif line.startswith("["):
            table = doc
            for part in line[1:line.index("]")].strip().split("."):
                table = table.setdefault(part, {})
        elif "=" in line:
            key, raw = line.split("=", 1)
            # An array written over several lines (`kept = [` on a voiceover).
            while depth(raw) > 0:
                more = next(lines, None)
                if more is None:
                    break
                raw += " " + uncommented(more)
            table[key.strip()] = value(raw)
    return doc


def uncommented(line):
    """`line` without its comment, which a `#` inside a string is not."""
    out = []
    for ch, in_string, _ in scan(line):
        if ch == "#" and not in_string:
            break
        out.append(ch)
    return "".join(out).strip()


def split_commas(s):
    """`s` split on its top-level commas: `[1, 2], [3, 4]` is two parts."""
    parts, cur = [], ""
    for ch, in_string, level in scan(s):
        if ch == "," and not in_string and level == 0:
            parts.append(cur)
            cur = ""
            continue
        cur += ch
    if cur.strip():
        parts.append(cur)
    return parts


def depth(s):
    """How many brackets `s` leaves open; those inside a string don't count."""
    open_brackets = 0
    for _, _, level in scan(s):
        open_brackets = level
    return open_brackets


def scan(s):
    """Each character of `s`, with whether it is inside a string and how many
    brackets are open at it."""
    quote, open_brackets = None, 0
    for ch in s:
        if quote:
            yield ch, True, open_brackets
            quote = None if ch == quote else quote
        elif ch in "\"'":
            quote = ch
            yield ch, True, open_brackets
        elif ch in "[{":
            open_brackets += 1
            yield ch, False, open_brackets
        elif ch in "]}":
            open_brackets -= 1
            yield ch, False, open_brackets
        else:
            yield ch, False, open_brackets


failed = []


def check(ok, what):
    print(("PASS " if ok else "FAIL ") + what, flush=True)
    if not ok:
        failed.append(what)


r = subprocess.run([SC, edit, "--check"], capture_output=True, text=True)
m = re.search(r"([\d:.]+) after cuts", r.stdout)
if r.returncode or not m:
    sys.exit("--check failed:\n" + r.stdout + r.stderr)
expect = secs(m.group(1))
print(r.stdout.strip().splitlines()[-1], flush=True)
if opts["--limit"] and expect > secs(opts["--limit"]):
    sys.exit(f"FAIL length {t(expect)} is over the limit {opts['--limit']} - not exporting")

if verify_only:
    check(os.path.exists(out), "existing export " + out)
else:
    r = subprocess.run([SC, edit, "--export", out], capture_output=True, text=True)
    check(r.returncode == 0 and os.path.exists(out), "export" + ("" if r.returncode == 0 else
          ": " + (r.stderr or r.stdout)[-500:]))
if failed:
    sys.exit(1)

p = json.loads(subprocess.run(
    ["ffprobe", "-v", "error", "-show_entries",
     "stream=codec_type,codec_name,profile,pix_fmt,width,height:format=duration,size,format_name",
     "-of", "json", out], capture_output=True, text=True, check=True).stdout)
v = next((s for s in p["streams"] if s["codec_type"] == "video"), {})
a = next((s for s in p["streams"] if s["codec_type"] == "audio"), {})
dur, size = float(p["format"]["duration"]), int(p["format"]["size"])
print(f"video {v.get('codec_name')} {v.get('profile')} {v.get('pix_fmt')} "
      f"{v.get('width')}x{v.get('height')}; audio {a.get('codec_name')} {a.get('profile')}; "
      f"{t(dur)}, {size / 1e6:.1f} MB", flush=True)
check(v.get("codec_name") == "h264" and v.get("pix_fmt") == "yuv420p", "video is H.264 yuv420p")
check(a.get("codec_name") == "aac", "audio is AAC")
check("mp4" in p["format"]["format_name"], "MP4 container")
check(abs(dur - expect) < 0.5, f"duration {t(dur)} matches --check {t(expect)}")
if opts["--limit"]:
    check(dur <= secs(opts["--limit"]), f"under the limit {opts['--limit']}")

doc = read_edit(edit)
cuts = sorted((secs(c["from"]), secs(c["to"]), c.get("note", "")) for c in doc.get("cut", []))


def edited(x):
    return x - sum(min(b, x) - a for a, b, _ in cuts if a < x)


frames = opts["--frames"] or os.path.splitext(out)[0] + ".frames"
os.makedirs(frames, exist_ok=True)
for k, s in enumerate(doc.get("slide", []), 1):
    a, b = edited(secs(s["from"])), edited(secs(s["to"]))
    png = os.path.join(frames, f"slide{k}.png")
    subprocess.run(["ffmpeg", "-nostdin", "-v", "error", "-y", "-ss", f"{(a + b) / 2:.2f}", "-i", out,
                    "-frames:v", "1", "-vf", "scale=640:-1", png], check=True)
    print(f"slide {k} ({s['file']} p{s.get('page', 1)}) {t(a)}-{t(b)}: {png}", flush=True)

end = secs(doc["source"]["duration"])
spots = [(edited(a), n) for a, b, n in cuts if n != "pause" and a > 0 and b < end - 0.01]
r = subprocess.run([SC, out, "--transcribe"], capture_output=True, text=True)
tj = os.path.splitext(out)[0] + ".transcript.json"
check(r.returncode == 0 and os.path.exists(tj), "transcribed the export" + ("" if r.returncode == 0
      else ": " + r.stderr[-300:]))
words = json.load(open(tj)) if os.path.exists(tj) else []
print("\nAt each cut (edited time, note, then what the export says; ‖ = the cut):")
for x, n in spots:
    near = [w for w in words if x - 4 < w["start"] < x + 4]
    text = ""
    for w in near:
        if text and w["start"] >= x and "‖" not in text:
            text += " ‖"
        text += " " + w["text"].strip()
    print(f"  {t(x)}  {n}\n          {text.strip()}")
print(("\nFAILED: " + "; ".join(failed)) if failed else "\nALL CHECKS PASSED")
sys.exit(1 if failed else 0)
