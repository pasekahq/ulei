"""The synthetic selection eval of the garden's related-fact hints (ADR-0096, report E15):
fictional people with 10 to 50 earlier facts (and a "large" set with 150 to 250) and new-fact
cases labelled with the earlier fact they duplicate, contradict or supersede, or none.

usage: gen.py OUT_DIR          writes the frozen base set: OUT_DIR/entities.jsonl, cases.jsonl
       gen.py --check DIR      regenerates it and fails unless DIR holds exactly that
       gen.py --all OUT_DIR    the whole E15 set (base and large) in E15's own form

Deterministic for a seed (default 20261126) under CPython's `random.Random`. Every name, place
and fact is invented. Labels are true by construction: one fact per single-valued slot, and an
event duplicate is unique; `gold` names the related earlier fact, empty for `none`.
"""
import json
import os
import random
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import bank  # noqa: E402  (the phrase bank beside this file)

GENDER_RE = re.compile(r"\[([^\]|]*)\|([^\]]*)\]")
PREP_G = re.compile(r"(?<![\w])([УуСсКк]|для|от|без|около) \{G\}")


def person_forms(p, lang, mode):
    """Placeholder values for a person in `lang` with subject mode full / first / pron."""
    g = p["gender"]
    if lang == "en":
        name = {"full": p["en_full"], "first": p["en_first"]}.get(mode)
        if name:
            return {"S": name, "s": name, "P": name + "'s", "p": name + "'s", "o": name}
        return ({"S": "He", "s": "he", "P": "His", "p": "his", "o": "him"} if g == "m"
                else {"S": "She", "s": "she", "P": "Her", "p": "her", "o": "her"})
    name = {"full": p["ru_full"], "first": p["ru_first"]}.get(mode)
    gen = {"full": p["ru_gen_full"], "first": p["ru_gen_first"]}.get(mode)
    if name:
        return {"S": name, "s": name, "G": gen, "Gp": gen}
    return ({"S": "Он", "s": "он", "G": "его", "Gp": "него"} if g == "m"
            else {"S": "Она", "s": "она", "G": "её", "Gp": "неё"})


def render(tpl, p, lang, mode, extra=None):
    f = person_forms(p, lang, mode)
    t = GENDER_RE.sub(lambda m: m.group(1) if p["gender"] == "m" else m.group(2), tpl)
    if lang == "ru":
        t = PREP_G.sub(lambda m: m.group(1) + " " + f["Gp"], t)
    for k, v in {**f, **(extra or {})}.items():
        t = t.replace("{" + k + "}", str(v))
    assert "{" not in t and "[" not in t, (tpl, t)
    return t[0].upper() + t[1:]


def normalise(t):
    return re.sub(r"[.!?…]+$", "", re.sub(r"\s+", " ", t.strip().casefold()))


def calendar(r):
    m = r.randrange(12)
    return {"mon": bank.MONTHS_EN[m], "monp": bank.MONTHS_RU_PREP[m], "mong": bank.MONTHS_RU_GEN[m],
            "yr": r.choice([2024, 2025, 2026]), "season": r.choice(bank.SEASONS_EN),
            "season_ru": r.choice(bank.SEASONS_RU)}


def subj_mode(r):
    return r.choices(["full", "first", "pron"], [45, 30, 25])[0]


def mark_change(tpl, lang, r):
    """Template with an explicit change-over-time marker."""
    pre = bank.CHANGE_EN_PRE if lang == "en" else bank.CHANGE_RU_PRE
    post = bank.CHANGE_EN_POST if lang == "en" else bank.CHANGE_RU_POST
    if tpl.startswith("{S}") and r.random() < 0.6:
        return r.choice(pre) + "{s}" + tpl[3:]
    return tpl.rstrip(".") + r.choice(post) + "."


def mark_neutral(tpl, lang, r):
    pre = bank.NEUTRAL_EN_PRE if lang == "en" else bank.NEUTRAL_RU_PRE
    x = r.choice(pre)
    if x and tpl.startswith("{S}"):
        return x + "{s}" + tpl[3:]
    return tpl


def event_text(kind, args, variant, lang, p, mode):
    ev = bank.EVENTS[kind]
    tpl = ev[lang][variant % len(ev[lang])]
    x = {}
    if kind == "trip":
        ci, m, yr = args
        en, rn, rp, ra = bank.CITIES[ci]
        x = {"c": en, "cp": rp, "ca": ra, "mon": bank.MONTHS_EN[m], "monp": bank.MONTHS_RU_PREP[m], "yr": yr}
    elif kind == "purchase":
        i, m, yr = args
        x = {"item": bank.ITEMS[i][0], "item_ru": bank.ITEMS[i][1], "mon": bank.MONTHS_EN[m],
             "monp": bank.MONTHS_RU_PREP[m], "yr": yr}
    elif kind == "meet":
        fi, pi, m = args
        x = {"fr": bank.FRIENDS[fi][0], "fr_ru": bank.FRIENDS[fi][1], "pl": bank.PLACES[pi][0],
             "pl_ru": bank.PLACES[pi][1], "mon": bank.MONTHS_EN[m], "monp": bank.MONTHS_RU_PREP[m]}
    elif kind == "concert":
        bi, m, yr = args
        x = {"band": bank.BANDS[bi][0], "band_ru": bank.BANDS[bi][1], "mon": bank.MONTHS_EN[m],
             "monp": bank.MONTHS_RU_PREP[m], "yr": yr}
    elif kind == "course":
        ci, m, yr = args
        x = {"course": bank.COURSES[ci][0], "course_ru": bank.COURSES[ci][1], "mon": bank.MONTHS_EN[m],
             "monp": bank.MONTHS_RU_PREP[m], "yr": yr}
    elif kind == "book":
        bi, m, yr = args
        x = {"book": bank.BOOKS[bi][0], "book_ru": bank.BOOKS[bi][1], "mon": bank.MONTHS_EN[m],
             "monp": bank.MONTHS_RU_PREP[m], "yr": yr}
    elif kind == "doctor":
        di, m, yr = args
        ru = bank.DOCTORS[di][1]
        x = {"doc": bank.DOCTORS[di][0], "doc_ru": ru, "doc_ru_dat": bank.DOCTOR_DAT[ru],
             "mon": bank.MONTHS_EN[m], "monp": bank.MONTHS_RU_PREP[m], "yr": yr}
    return render(tpl, p, lang, mode, x)


def people(r):
    out = []
    for name, g in bank.EN_PEOPLE:
        out.append({"mode": "en", "canonical": name, "gender": g, "en_full": name,
                    "en_first": name.split()[0]})
    for full, gen, first, firstgen, g in bank.RU_PEOPLE:
        out.append({"mode": "ru", "canonical": full, "gender": g, "ru_full": full, "ru_gen_full": gen,
                    "ru_first": first, "ru_gen_first": firstgen})
    for full, gen, first, firstgen, g, latin, canon in bank.MIXED_PEOPLE:
        out.append({"mode": "mixed", "canonical": full if canon == "cyr" else latin, "gender": g,
                    "ru_full": full, "ru_gen_full": gen, "ru_first": first, "ru_gen_first": firstgen,
                    "en_full": latin, "en_first": latin.split()[0]})
    return out


def lang_of(p, r):
    return p["mode"] if p["mode"] != "mixed" else r.choice(["en", "ru"])


def build_entity(p, eid, n_hist, r, n_cases_plan):
    slots = list(bank.SLOTS)
    max_state = len(slots) - 6
    n_state = min(max_state, max(4, round(n_hist * r.uniform(0.45, 0.75))))
    chosen = r.sample(slots, n_state)
    facts = []
    for s in chosen:
        vi = r.randrange(len(bank.SLOTS[s]["values"]))
        lang = lang_of(p, r)
        variants = bank.SLOTS[s]["values"][vi][lang]
        tpl = mark_neutral(r.choice(variants), lang, r)
        facts.append({"slot": s, "value": vi, "lang": lang,
                      "text": render(tpl, p, lang, subj_mode(r), calendar(r))})
    seen_events = set()
    while len(facts) < n_hist:
        kind = r.choice(list(bank.EVENTS))
        args = bank.EVENTS[kind]["args"](r)
        if (kind, args) in seen_events:
            continue
        seen_events.add((kind, args))
        lang = lang_of(p, r)
        facts.append({"slot": "event:" + kind, "event": [kind, list(args)], "lang": lang,
                      "text": event_text(kind, args, r.randrange(3), lang, p, subj_mode(r))})
    r.shuffle(facts)
    norms = set()
    for i, f in enumerate(facts):
        f["id"] = f"{eid}.f{i}"
        norms.add(normalise(f["text"]))

    cases = []
    used = set()
    state_facts = [f for f in facts if not f["slot"].startswith("event:")]
    event_facts = [f for f in facts if f["slot"].startswith("event:")]
    hist_slots = {f["slot"] for f in state_facts}
    absent = [s for s in slots if s not in hist_slots]
    hist_groups = {bank.SLOTS[s]["group"] for s in hist_slots}

    def new_case(label, text, lang, gold=None, slot=None, value=None):
        if normalise(text) in norms:
            return False
        cases.append({"entity": eid, "label": label, "text": text, "lang": lang,
                      "gold": gold["id"] if gold else None, "slot": slot,
                      "gold_lang": gold["lang"] if gold else None, "value": value})
        norms.add(normalise(text))
        return True

    for label in n_cases_plan:
        for _attempt in range(30):
            lang = lang_of(p, r)
            mode = subj_mode(r)
            if label in ("duplicate", "contradiction", "supersedes"):
                pool = [f for f in state_facts if f["id"] not in used
                        and (label != "supersedes" or bank.SLOTS[f["slot"]]["change"])]
                if not pool:
                    break
                g = r.choice(pool)
                vals = bank.SLOTS[g["slot"]]["values"]
                if label == "duplicate":
                    vi = g["value"]
                    tpl = mark_neutral(r.choice(vals[vi][lang]), lang, r)
                else:
                    vi = r.choice([i for i in range(len(vals)) if i != g["value"]])
                    tpl = r.choice(vals[vi][lang])
                    tpl = mark_change(tpl, lang, r) if label == "supersedes" else mark_neutral(tpl, lang, r)
                if new_case(label, render(tpl, p, lang, mode, calendar(r)), lang, g, g["slot"], vi):
                    used.add(g["id"])
                    break
            elif label == "duplicate_event":
                pool = [f for f in event_facts if f["id"] not in used]
                if not pool:
                    break
                g = r.choice(pool)
                kind, args = g["event"]
                if new_case("duplicate", event_text(kind, tuple(args), r.randrange(3), lang, p, mode),
                            lang, g, g["slot"]):
                    used.add(g["id"])
                    break
            elif label == "none_hard":
                pool = [s for s in absent if bank.SLOTS[s]["group"] in hist_groups
                        or any(n in hist_groups for n in bank.NEAR_GROUPS.get(bank.SLOTS[s]["group"], []))]
                pool = pool or absent
                s = r.choice(pool)
                vi = r.randrange(len(bank.SLOTS[s]["values"]))
                tpl = mark_neutral(r.choice(bank.SLOTS[s]["values"][vi][lang]), lang, r)
                if new_case("none", render(tpl, p, lang, mode, calendar(r)), lang, None, s, vi):
                    cases[-1]["none_kind"] = "hard"
                    absent.remove(s)
                    break
            elif label == "none_easy":
                kind = r.choice(list(bank.EVENTS))
                args = bank.EVENTS[kind]["args"](r)
                if (kind, args) in seen_events:
                    continue
                if new_case("none", event_text(kind, args, r.randrange(3), lang, p, mode), lang, None,
                            "event:" + kind):
                    cases[-1]["none_kind"] = "event"
                    seen_events.add((kind, args))
                    break
    return facts, cases


def generate(seed=20261126):
    """The whole set: entities and cases, base and large."""
    r = random.Random(seed)
    ppl = people(r)
    # the base set: every person; a large set: 4 people per mode get a 150-250 fact history
    large = set()
    for mode in ("en", "ru", "mixed"):
        large.update(r.sample([i for i, p in enumerate(ppl) if p["mode"] == mode], 4))
    ents, cases = [], []
    for i, p in enumerate(ppl):
        eid = f"e{i:02d}"
        big = i in large
        n_hist = r.randint(150, 250) if big else r.randint(10, 50)
        plan = ["duplicate", "contradiction", "supersedes",
                r.choice(["duplicate", "contradiction", "supersedes"]),
                "duplicate_event", "none_hard", "none_easy"]
        facts, cs = build_entity(p, eid, n_hist, r, plan)
        ents.append({"id": eid, "name": p["canonical"], "mode": p["mode"], "set": "large" if big else "base",
                     "facts": facts})
        for c in cs:
            c["set"] = "large" if big else "base"
        cases.extend(cs)
    for j, c in enumerate(cases):
        c["id"] = f"c{j:03d}"
    return ents, cases


def frozen(ents, cases):
    """The two files of the frozen base set as text: per entity its id, name, mode and earlier
    facts in order; per case its id, entity, label, language, gold fact and text."""
    e_lines = [json.dumps({"id": e["id"], "name": e["name"], "mode": e["mode"],
                           "facts": [[f["id"], f["text"]] for f in e["facts"]]}, ensure_ascii=False)
               for e in ents if e["set"] == "base"]
    c_lines = [json.dumps({"id": c["id"], "entity": c["entity"], "label": c["label"], "lang": c["lang"],
                           "gold": c.get("gold") or "", "text": c["text"]}, ensure_ascii=False)
               for c in cases if c["set"] == "base"]
    return {"entities.jsonl": "\n".join(e_lines) + "\n", "cases.jsonl": "\n".join(c_lines) + "\n"}


def write_all(out, ents, cases):
    os.makedirs(out, exist_ok=True)
    with open(os.path.join(out, "entities.jsonl"), "w", encoding="utf-8") as fh:
        for e in ents:
            fh.write(json.dumps(e, ensure_ascii=False) + "\n")
    with open(os.path.join(out, "cases.jsonl"), "w", encoding="utf-8") as fh:
        for c in cases:
            fh.write(json.dumps(c, ensure_ascii=False) + "\n")


def main():
    args = sys.argv[1:]
    if len(args) != 2 and not (len(args) == 1 and not args[0].startswith("--")):
        sys.exit(__doc__)
    ents, cases = generate()
    if args[0] == "--all":
        write_all(args[1], ents, cases)
        return
    files = frozen(ents, cases)
    if args[0] == "--check":
        for name, text in files.items():
            with open(os.path.join(args[1], name), encoding="utf-8") as fh:
                if fh.read() != text:
                    sys.exit(f"{name} differs from what gen.py makes; regenerate it")
        print("reconcile eval: the frozen set matches the generator")
        return
    os.makedirs(args[0], exist_ok=True)
    for name, text in files.items():
        with open(os.path.join(args[0], name), "w", encoding="utf-8") as fh:
            fh.write(text)


if __name__ == "__main__":
    main()
