#!/usr/bin/env python3
"""H0 criticism detector: train a tiny char-trigram logistic regression.

Input:  bench/criticism/train.jsonl  ({"text": str, "label": 0|1})
Output: src/agent/criticism_weights.json (sparse, versioned)

Stdlib only, seeded, deterministic. The normalization and the FNV-1a hash
below MUST stay byte-identical to src/agent/criticism.rs — the comments
mark every mirrored spot. Change one side, change both.

Usage:  python bench/criticism/train.py
"""
import json
import math
import random
import sys
from pathlib import Path

HERE = Path(__file__).parent
DATA = HERE / "train.jsonl"
OUT = Path("src/agent/criticism_weights.json")

DIM = 1 << 16
EPOCHS = 60
LR = 0.5
L2 = 3e-4
SPARSE_CUTOFF = 1e-4
SEED = 42

FNV_OFFSET = 14695981039346656037
FNV_PRIME = 1099511628211
MASK64 = 0xFFFFFFFFFFFFFFFF


def normalize(text):  # MIRRORED in criticism.rs::normalize
    text = text.lower().replace("ё", "е")
    out = []
    run_char = None
    run_len = 0
    for ch in text:
        if ch == run_char:
            run_len += 1
        else:
            run_char = ch
            run_len = 1
        # elongation cap: runs longer than 2 collapse ("сломааал" ~ "сломаал",
        # still distinct from "сломал" — typo tolerance without flattening)
        if run_len <= 2:
            out.append(ch)
    return "".join(out)


def fnv1a64(data: bytes) -> int:  # MIRRORED in criticism.rs::fnv1a64
    h = FNV_OFFSET
    for b in data:
        h ^= b
        h = (h * FNV_PRIME) & MASK64
    return h


def trigrams(text):  # MIRRORED in criticism.rs::features
    padded = " " + normalize(text) + " "
    chars = list(padded)
    return ["".join(chars[i : i + 3]) for i in range(len(chars) - 2)]


def feats(text):
    return [fnv1a64(t.encode("utf-8")) % DIM for t in trigrams(text)]


def sigmoid(x):
    if x < -30:
        return 0.0
    if x > 30:
        return 1.0
    return 1.0 / (1.0 + math.exp(-x))


def train(rows):
    w = [0.0] * DIM
    b = 0.0
    rng = random.Random(SEED)
    featurized = [(feats(r["text"]), float(r["label"])) for r in rows]
    for _ in range(EPOCHS):
        rng.shuffle(featurized)
        for xs, y in featurized:
            s = b + sum(w[i] for i in xs)
            p = sigmoid(s)
            err = p - y
            lr = LR
            for i in set(xs):
                w[i] -= lr * (err * xs.count(i) + L2 * w[i])
            b -= lr * err
    return w, b


def score(w, b, text):
    return sigmoid(b + sum(w[i] for i in feats(text)))


def typo(text, rng):
    """One synthetic typo: drop/swap/repeat a middle char. Eval only."""
    chars = list(text)
    if len(chars) < 5:
        return text
    i = rng.randrange(2, len(chars) - 1)
    op = rng.randrange(3)
    if op == 0:
        del chars[i]
    elif op == 1:
        chars[i], chars[i + 1] = chars[i + 1], chars[i]
    else:
        chars.insert(i, chars[i])
    return "".join(chars)


def main():
    rows = [json.loads(l) for l in DATA.read_text(encoding="utf-8").splitlines() if l.strip()]
    assert len(rows) >= 50, "too few training rows"

    # 5-fold CV doubles as the honest estimate AND the out-of-fold pool
    # for thresholds: in-sample scores are saturated (LR overfits the
    # small set), so fire/maybe cutoffs picked on train would be fantasy.
    rng = random.Random(SEED)
    idx = list(range(len(rows)))
    rng.shuffle(idx)
    folds = [idx[i::5] for i in range(5)]
    oof = []
    oof_by_row = {}
    cv_correct = cv_total = 0
    for k in range(5):
        test = set(folds[k])
        tr = [rows[i] for i in idx if i not in test]
        wk, bk = train(tr)
        for i in folds[k]:
            p = score(wk, bk, rows[i]["text"])
            oof.append((p, rows[i]["label"]))
            oof_by_row[i] = p
            cv_correct += (p >= 0.5) == bool(rows[i]["label"])
            cv_total += 1
    if "--errors" in sys.argv:
        for i in idx:
            p = oof_by_row[i]
            if (p >= 0.5) != bool(rows[i]["label"]):
                print(f"  want={rows[i]['label']} p={p:.2f} {rows[i]['text']}")
        return

    # thresholds on OOF: fire at ~0.95 precision, maybe at ~0.90 recall
    oof_sorted = sorted(oof, reverse=True)
    n_pos = sum(1 for _, y in oof_sorted if y == 1)
    fire_t, maybe_t = 0.9, 0.5
    tp = fp = 0
    for s, y in oof_sorted:
        if y == 1:
            tp += 1
        else:
            fp += 1
        if fp > 0 and tp / (tp + fp) < 0.95:
            break
        fire_t = s
    tp = 0
    for s, y in oof_sorted:
        if y == 1:
            tp += 1
        if tp / n_pos >= 0.90:
            maybe_t = s
            break
    maybe_t = min(maybe_t, fire_t - 0.05)

    # final model on all rows (thresholds stay OOF-honest)
    w, b = train(rows)

    # typo robustness probe on positives
    rng = random.Random(SEED + 1)
    pos = [r["text"] for r in rows if r["label"] == 1]
    survived = sum(1 for t in pos if score(w, b, typo(t, rng)) >= maybe_t)

    acc = sum(1 for s, y in oof if (s >= 0.5) == bool(y)) / len(oof)
    print(f"rows={len(rows)} pos={n_pos} oof-acc={acc:.3f} cv-acc={cv_correct/cv_total:.3f}")
    print(f"thresholds: fire>={fire_t:.3f} maybe>={maybe_t:.3f}")
    print(f"typo probe: {survived}/{len(pos)} positives still >= maybe after 1 synthetic typo")

    sparse = {str(i): round(v, 6) for i, v in enumerate(w) if abs(v) >= SPARSE_CUTOFF}
    payload = {
        "version": 1,
        "dim": DIM,
        "bias": round(b, 6),
        "weights": sparse,
        "threshold_fire": round(fire_t, 4),
        "threshold_maybe": round(maybe_t, 4),
        "trained_on": len(rows),
    }
    OUT.write_text(json.dumps(payload, ensure_ascii=False), encoding="utf-8")
    print(f"wrote {OUT} ({OUT.stat().st_size // 1024}KB, {len(sparse)} nonzero)")


if __name__ == "__main__":
    sys.exit(main())
