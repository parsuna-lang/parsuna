"""Sphinx configuration for the parsuna documentation."""

project = "parsuna"
author = "parsuna authors"
release = "0.1.0"

extensions = [
    "sphinx.ext.autosectionlabel",
]

autosectionlabel_prefix_document = True

exclude_patterns = ["_build", "Thumbs.db", ".DS_Store", ".venv"]

master_doc = "index"

html_theme = "furo"

highlight_language = "text"
pygments_style = "friendly"

rst_prolog = """
.. role:: parsuna(code)
   :language: text
"""
