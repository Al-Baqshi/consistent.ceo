"""LoRA fine-tune of Gemma 4 E2B (default; --base for E4B) on Consistent transcripts.

Needs an NVIDIA GPU (a free Colab T4 is enough for E2B); the dev Mac (Intel +
AMD) cannot train. Typical use:

    pip install -r requirements-gpu.txt
    python train_unsloth.py --data data/sft/train.jsonl --eval data/sft/val.jsonl \
        --out out/consistent-gemma --gguf q4_k_m

Rows are {"messages": [...], "tools": [...]?} (see distill.py). Loss is taken
only on what the model itself must produce: its replies and tool calls. Tool
results — which Gemma 4 folds inside the model turn — are masked, so the
model never learns to invent them.
"""

import argparse
import json
import re

import torch
from datasets import Dataset
from transformers import Trainer, TrainingArguments
from unsloth import FastModel

MODEL_TURN = "<|turn>model\n"
TURN_END = "<turn|>"
TOOL_RESP_OPEN = "<|tool_response>"
TOOL_RESP_CLOSE = "<tool_response|>"


def load_jsonl(path):
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def to_hf_messages(messages):
    """Training rows -> the message shape Gemma 4's chat template expects."""
    out, names = [], {}
    for m in messages:
        m = {k: v for k, v in m.items() if v is not None}
        if m.get("tool_calls"):
            calls = []
            for c in m["tool_calls"]:
                fn = c.get("function") or c
                args = fn.get("arguments", {})
                if isinstance(args, str):
                    args = json.loads(args or "{}")
                names[c.get("id")] = fn["name"]
                calls.append({"type": "function", "function": {"name": fn["name"], "arguments": args}})
            m["tool_calls"] = calls
        if m["role"] == "tool":
            m["name"] = m.get("name") or names.get(m.get("tool_call_id"), "unknown")
        out.append(m)
    return out


def trainable_spans(text):
    """Character spans the loss applies to: model output, minus tool results."""
    spans = []
    for m in re.finditer(re.escape(MODEL_TURN), text):
        start = m.end()
        end = text.find(TURN_END, start)
        end = len(text) if end < 0 else end + len(TURN_END)
        cursor = start
        while True:
            open_at = text.find(TOOL_RESP_OPEN, cursor, end)
            if open_at < 0:
                spans.append((cursor, end))
                break
            # The opening tag is the model's own "now wait for the tool" token.
            spans.append((cursor, open_at + len(TOOL_RESP_OPEN)))
            close_at = text.find(TOOL_RESP_CLOSE, open_at, end)
            if close_at < 0:
                break
            cursor = close_at + len(TOOL_RESP_CLOSE)
    return spans


def tokenize_with_labels(tok, text, max_len):
    enc = tok(text, add_special_tokens=False, return_offsets_mapping=True, truncation=True, max_length=max_len)
    spans = trainable_spans(text)
    labels = []
    for tid, (a, b) in zip(enc["input_ids"], enc["offset_mapping"]):
        keep = any(a >= s and b <= e and b > a for s, e in spans)
        labels.append(tid if keep else -100)
    return {"input_ids": enc["input_ids"], "labels": labels}


class Collator:
    def __init__(self, pad_id):
        self.pad_id = pad_id

    def __call__(self, batch):
        n = max(len(b["input_ids"]) for b in batch)
        ids = torch.full((len(batch), n), self.pad_id, dtype=torch.long)
        labels = torch.full((len(batch), n), -100, dtype=torch.long)
        mask = torch.zeros((len(batch), n), dtype=torch.long)
        for i, b in enumerate(batch):
            k = len(b["input_ids"])
            ids[i, :k] = torch.tensor(b["input_ids"])
            labels[i, :k] = torch.tensor(b["labels"])
            mask[i, :k] = 1
        return {"input_ids": ids, "labels": labels, "attention_mask": mask}


def main():
    p = argparse.ArgumentParser()
    # E2B: ~2x faster than E4B on CPU-only laptops, trains on a free 16 GB T4.
    p.add_argument("--base", default="unsloth/gemma-4-E2B-it")
    p.add_argument("--data", required=True, nargs="+")
    p.add_argument("--eval")
    p.add_argument("--out", default="out/consistent-gemma")
    p.add_argument("--max-seq", type=int, default=8192)
    p.add_argument("--epochs", type=float, default=2)
    p.add_argument("--lr", type=float, default=1e-4)
    p.add_argument("--rank", type=int, default=16)
    p.add_argument("--batch", type=int, default=1)
    p.add_argument("--grad-accum", type=int, default=8)
    # 4-bit QLoRA by default; E2B in plain LoRA (--no-4bit) still fits ~10 GB.
    p.add_argument("--no-4bit", action="store_true")
    p.add_argument("--gguf", default="q4_k_m", help="'' to skip GGUF export")
    p.add_argument("--push", help="optional HF repo id for the GGUF")
    args = p.parse_args()

    model, processor = FastModel.from_pretrained(
        model_name=args.base,
        max_seq_length=args.max_seq,
        load_in_4bit=not args.no_4bit,
        full_finetuning=False,
    )
    tok = getattr(processor, "tokenizer", processor)
    model = FastModel.get_peft_model(
        model,
        finetune_vision_layers=False,
        finetune_language_layers=True,
        finetune_attention_modules=True,
        finetune_mlp_modules=True,
        r=args.rank,
        lora_alpha=args.rank,
        lora_dropout=0,
        bias="none",
        random_state=3407,
    )

    # Google's own chat template (shipped with the checkpoint) is what
    # Ollama's gemma4 renderer reproduces, so we train on exactly that.
    def render(row):
        return tok.apply_chat_template(
            to_hf_messages(row["messages"]),
            tools=[{"type": "function", "function": t} for t in row["tools"]] if row.get("tools") else None,
            tokenize=False,
            add_generation_prompt=False,
            enable_thinking=False,
        )

    def build(paths):
        rows = [r for path in paths for r in load_jsonl(path)]
        items, dropped = [], 0
        for r in rows:
            item = tokenize_with_labels(tok, render(r), args.max_seq)
            if all(l == -100 for l in item["labels"]):
                dropped += 1  # truncated before the answer — useless
                continue
            items.append(item)
        print(f"{len(items)} examples ({dropped} dropped: answer beyond max-seq)")
        return Dataset.from_list(items)

    first = load_jsonl(args.data[0])[0]
    sample = render(first)
    print("---- rendered sample ----\n" + sample[-3000:])
    print("---- trained-on text ----")
    for s, e in trainable_spans(sample):
        print(repr(sample[s:e][:600]))

    train_ds = build(args.data)
    eval_ds = build([args.eval]) if args.eval else None

    trainer = Trainer(
        model=model,
        train_dataset=train_ds,
        eval_dataset=eval_ds,
        data_collator=Collator(tok.pad_token_id if tok.pad_token_id is not None else 0),
        args=TrainingArguments(
            output_dir=args.out + "/checkpoints",
            per_device_train_batch_size=args.batch,
            per_device_eval_batch_size=1,
            gradient_accumulation_steps=args.grad_accum,
            num_train_epochs=args.epochs,
            learning_rate=args.lr,
            lr_scheduler_type="cosine",
            warmup_ratio=0.05,
            weight_decay=0.01,
            optim="adamw_8bit",
            logging_steps=5,
            eval_strategy="steps" if eval_ds is not None else "no",
            eval_steps=50,
            save_strategy="steps",
            save_steps=100,
            save_total_limit=2,
            bf16=torch.cuda.is_bf16_supported(),
            fp16=not torch.cuda.is_bf16_supported(),
            remove_unused_columns=False,
            report_to="none",
            seed=3407,
        ),
    )
    trainer.train()

    model.save_pretrained(args.out + "/lora")
    processor.save_pretrained(args.out + "/lora")
    if args.gguf:
        model.save_pretrained_gguf(args.out + "/gguf", processor, quantization_method=args.gguf)
        if args.push:
            model.push_to_hub_gguf(args.push, processor, quantization_method=args.gguf)


if __name__ == "__main__":
    main()
