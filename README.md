# learnive

learnive turns books and articles into interactible HTML rich living documents.
the document is divided into atomic knowledge nodes of explanations followed by
an exercise where, if you fail the exercise, you are given a commented solution
and a new similar exercise. after finishing a node spaced repetition reviews are
scheduled.

at any point while reading the live document you can ask a question that'll be
answered with the content of the book and, if the question falls out of the
scope of the book and any book in your library, you'll be prompted to acquire
the relevant book to answer it.

all information in the living document is grounded in the book and cited
accordingly. if at any point you want to check the information at the source you
can click the citation an open the relevant page of the book inside the
application.

## Get the app

**Release binary (easiest):** download the archive for your platform from [GitHub Releases](https://github.com/Hashino/learnive/releases), unpack it, and run the `learnive` binary inside.

**From source:** with a recent Rust toolchain (edition 2024, Rust 1.85+),

```sh
cargo install --path crates/learnive
```

## Run

```sh
learnive
```

(Or `cargo run` for a dev build.) It opens your default browser at a
token-authenticated URL automatically. The token is required on every request;
if the browser doesn't open, use the URL printed to the console:

```
http://127.0.0.1:7420/?token=<generated-token>
```


## Configure the AI (Groq, free)

learnive is bring-your-own-AI and has been validated to work reasonably well
with Groq's free models (`openai/gpt-oss-20b` for the fast tier,
`openai/gpt-oss-120b` for the robust tier).

### 1. Get a Groq API key

1. Go to [console.groq.com](https://console.groq.com) and sign up or log in (Google/GitHub/email).
2. Open [API Keys](https://console.groq.com/keys) and click **Create API Key**.
3. Copy the key immediately — Groq only shows it once. It starts with `gsk_`.

### 2. Enter the key in learnive

**In the app:** click the gear icon in the left panel → **Provider** section → provider **Groq** → paste the key → **Show advanced options** and set fast-tier model to `openai/gpt-oss-20b` and robust-tier model to `openai/gpt-oss-120b`  → **Save**. The app validates the key *and* both models against the real endpoint before saving.

## Use it

1. **Add your PDFs.** Copy the books/articles for the topic into the library folder (`<data>/library/`, i.e. `learnive-data/library/` by default). The manual picker and the library-check panel both show this path with a button to copy it.
2. **Start a curriculum.** Type a topic into "What are we learning?" and press Start (the app proposes a reading list from your library), or click "I already know what I want to learn" to pick the exact books yourself — no model calls involved in the manual path.
3. **Confirm the outline**, then pass the library check (every item matched to a PDF with a readable table of contents).
4. **Read and answer.** Each node streams explanatory prose grounded in your sources (click a citation to open the PDF at that page), then ends in a comprehension check graded against a rubric locked at generation time. Passing advances; failing opens a remediation conversation with a worked example and a new problem.
5. **Ask anytime** via the ask bar — the answer is grounded in your library the same way, and can spawn a sub-node in the document.

> [!NOTE]
> if you're using free models, it is recommended to use a free chatbot like
> Claude or Chat GPT to generate the reading list and use the manual "I already
> know what I want to learn" mode with that reading list.
