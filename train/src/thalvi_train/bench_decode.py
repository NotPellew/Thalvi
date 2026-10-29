"""Benchmarks stock against CUDA-graph Glimmer decoding on one GPU: speed and token agreement.

Greedy-decodes fixed prompts, one with a forced chunk mid-way, through the stock path
with and without bf16 autocast and through the static decoder, and prints tokens per
second of each and the first token where the static run departs from the stock one.
Along the stock tokens it also prints the logit error of the static run and, as a noise
floor, of the stock path with eager attention, with the stock top-two margin at every
position where the arg-max flips: near-ties flip under any kernel change in bf16.
"""

import argparse
import time
from pathlib import Path

import torch
from transformers import AutoTokenizer

from thalvi_train.decode import decode_steps, open_run
from thalvi_train.glimmer import load_glimmer
from thalvi_train.prompts import GlimmerTokenizer
from thalvi_train.static_decode import DEFAULT_CAPACITY, StaticDecoder

TOPICS = (
    "the history of the printing press and how it changed European society",
    "how a modern CPU executes an instruction, from fetch to retire",
    "the water cycle and its role in regional climate",
)
FORCED_TEXT = "\nLet me double check that before answering.\n"
FORCED_AFTER = 24


def prompt_ids(tokenizer: GlimmerTokenizer, topic: str) -> list[int]:
    user = f"Write a detailed, well-structured essay about {topic}. " + "Be thorough. " * 40
    return tokenizer.chat_ids("You are a careful assistant.", user)


def decode(
    glimmer, ids: list[int], count: int, forced: list[int] | None
) -> tuple[list[int], float]:
    """Greedy tokens and the seconds per token after the first, forcing `forced` once if given."""
    tensor = torch.tensor([ids])
    steps = decode_steps(glimmer, glimmer.embed_text(tensor), torch.ones_like(tensor), count, -1)
    tokens = [next(steps)[0]]
    torch.cuda.synchronize()
    started = time.perf_counter()
    try:
        while True:
            send = forced if forced and len(tokens) == FORCED_AFTER else None
            tokens.append(steps.send(send)[0])
    except StopIteration:
        pass
    torch.cuda.synchronize()
    return tokens, (time.perf_counter() - started) / (len(tokens) - 1)


def trace(glimmer, ids: list[int], tokens: list[int]) -> torch.Tensor:
    """Logits `(len(tokens), vocab)` after the prompt and after each of `tokens` but the last."""
    tensor = torch.tensor([ids])
    run = open_run(glimmer, glimmer.embed_text(tensor), torch.ones_like(tensor), len(tokens))
    try:
        out = [run.start(glimmer.embed_text(tensor))]
        out += [run.feed(torch.tensor([[token]])).clone() for token in tokens[:-1]]
        return torch.cat(out)
    finally:
        run.close()


def compare(stock: torch.Tensor, other: torch.Tensor) -> str:
    """Size of the logit error of `other`, and stock's top-two margin where the arg-maxes differ."""
    top = stock.topk(2, dim=-1).values
    margin = top[:, 0] - top[:, 1]
    flips = (stock.argmax(-1) != other.argmax(-1)).nonzero().flatten().tolist()
    delta = (stock - other).abs().max(dim=-1).values
    return (
        f"max|dlogit| median {delta.median():.3f} max {delta.max():.3f}; "
        f"flips {[(i, round(margin[i].item(), 3)) for i in flips]}"
    )


def report_gaps(glimmer, ids: list[int], tokens: list[int]) -> str:
    """Static and, as a noise floor, eager-attention logits against stock, along `tokens`."""
    static, glimmer.static = glimmer.static, None
    stock = trace(glimmer, ids, tokens)
    config = glimmer.decoder.config
    config._attn_implementation = "eager"
    eager = trace(glimmer, ids, tokens)
    config._attn_implementation = "sdpa"
    glimmer.static = static
    fast = compare(stock, trace(glimmer, ids, tokens))
    return f"static: {fast} | eager attention: {compare(stock, eager)}"


def first_difference(a: list[int], b: list[int]) -> int | None:
    return next((i for i, (x, y) in enumerate(zip(a, b, strict=False)) if x != y), None)


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--glimmer", type=Path, required=True)
    parser.add_argument("--tokens", type=int, default=200)
    parser.add_argument("--compile", action="store_true")
    args = parser.parse_args(argv)
    glimmer = load_glimmer(args.glimmer, device="cuda")
    tokenizer = GlimmerTokenizer(AutoTokenizer.from_pretrained(args.glimmer))
    forced = tokenizer.text_ids(FORCED_TEXT)
    cases = [(prompt_ids(tokenizer, topic), None) for topic in TOPICS]
    cases.append((prompt_ids(tokenizer, TOPICS[0]), forced))
    cases.append(([*prompt_ids(tokenizer, TOPICS[1])] * 4, None))
    print(f"[bench] prompts {[len(ids) for ids, _ in cases]} tokens", flush=True)

    decode(glimmer, cases[0][0], 8, None)
    stock, plain = [], []
    for ids, send in cases:
        with torch.autocast("cuda", dtype=torch.bfloat16):
            stock.append(decode(glimmer, ids, args.tokens, send))
        plain.append(decode(glimmer, ids, args.tokens, send))
    started = time.monotonic()
    glimmer.static = StaticDecoder(glimmer, DEFAULT_CAPACITY, compile=args.compile)
    glimmer.static.warm_up()
    print(f"[bench] warm-up {time.monotonic() - started:.1f} s", flush=True)
    for index, (ids, send) in enumerate(cases):
        fast = decode(glimmer, ids, args.tokens, send)
        print(
            f"[bench] case {index}: stock+autocast {1 / stock[index][1]:.1f}/s,"
            f" stock {1 / plain[index][1]:.1f}/s, static {1 / fast[1]:.1f}/s;"
            f" first difference vs stock+autocast {first_difference(stock[index][0], fast[0])},"
            f" vs stock {first_difference(plain[index][0], fast[0])},"
            f" stock modes {first_difference(stock[index][0], plain[index][0])}",
            flush=True,
        )
        print(
            f"[bench] case {index} gaps: {report_gaps(glimmer, ids, stock[index][0])}", flush=True
        )


if __name__ == "__main__":
    main()
