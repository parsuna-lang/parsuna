Pipeline
========

This part of the manual follows a grammar from its source form all the
way to emitted target code, documenting each pass the generator runs
and the data structure it produces.

The high-level chain is:

.. parsed-literal::

    .parsuna source
         │
         │  :doc:`parse`
         ▼
    Grammar IR        (in-memory AST of tokens and rules)
         │
         │  :doc:`analyze`
         ▼
    AnalyzedGrammar   (Grammar plus FIRST/FOLLOW/nullable, chosen k)
         │
         │  :doc:`lower` — build → layout → optimize → validate
         ▼
    StateTable        (flat state machine + per-mode lexer DFAs)
         │
         │  :doc:`codegen`
         ▼
    EmittedFile[]     (target-language source)

Each step is a pure function with a well-typed input and output — no
shared mutable state, no side effects. The separation is deliberate:
it means the backends are small (they only read a state table), it
means the analysis is reusable (debug dumps, tree-sitter export, and
code generation all share it), and it means you can reason about the
generator one pass at a time.

The rest of this section treats each pass in detail.

.. toctree::
   :maxdepth: 1

   parse
   analyze
   lower
   codegen
