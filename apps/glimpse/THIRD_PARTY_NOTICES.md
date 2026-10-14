# Third-party notices

Production Glimpse code is original to this project. Its standard dependencies
retain their respective licenses: GTK/GLib, SQLite/rusqlite, notify, serde, and the
other crates listed in Cargo.lock.

Optional inference uses unmodified llama.cpp at commit
4d756bc72bf00a4aacf410ae15a2d315f3db400d (MIT). Runtime setup includes its LICENSE.
EmbeddingGemma 2 weights are published under Apache 2.0; GGUF artifacts are pinned to
revision bfcd298762cc34d0357ece5ebdd31791a3a374d8 and checked by SHA-256.
See https://huggingface.co/google/embeddinggemma-2 and
https://huggingface.co/ggml-org/embeddinggemma-2-GGUF.

Development-only reference fixtures retain their separate notice and MIT license
under tests/data. No Recall engine, extraction, inference wrapper, or ranking code
is included in production.
