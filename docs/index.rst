Parsuna
=======

Parsuna is a parser generator. You write a grammar in a small, regular-like
DSL and it emits a pull-based, recoverable parser in the target language of
your choice. The generator supports Rust, Python, TypeScript, Go, Java, C#,
and C; every target shares the same event model, so a program that consumes
a parse in one language is structurally the same in every other.

This manual is split into three parts:

* A language-agnostic **user guide** covering the grammar DSL, the CLI, and
  the event stream consumers iterate over.
* A **pipeline reference** that follows one grammar from source through
  parsing, analysis, lowering, and code generation, documenting the data
  each pass produces.
* Backend-shaped **appendices** for mapping the shared event stream to
  idioms in each target language.

.. toctree::
   :maxdepth: 2
   :caption: User guide

   introduction
   grammar_language
   usage
   event_model

.. toctree::
   :maxdepth: 2
   :caption: Pipeline

   pipeline/index
   pipeline/parse
   pipeline/analyze
   pipeline/lower
   pipeline/codegen

Indices
-------

* :ref:`genindex`
* :ref:`search`
