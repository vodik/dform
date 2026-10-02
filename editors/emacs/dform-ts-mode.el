;;; dform-ts-mode.el --- Major mode for dform (.df) files -*- lexical-binding: t; -*-

;; Copyright (C) 2026 Simon Gomizelj

;; Author: Simon Gomizelj
;; Keywords: languages
;; Package-Requires: ((emacs "29.1"))
;; Version: 0.1.0

;; This file is not part of GNU Emacs.

;;; Commentary:

;; A `treesit' major mode for dform's `.df' files (docs/grammar.md in
;; the dform repository), backed by the editor grammar in
;; tree-sitter-dform/.  The compiler keeps its own parser
;; (src/syntax/); this grammar and mode are for editing only.
;;
;; Provides:
;; - Font-lock, translated from tree-sitter-dform/queries/highlights.scm.
;;   A dot in a field-value position (proposal G, G-6) is captured as a
;;   reference, not a read, and is fontified with `dform-reference-face'.
;; - Indentation, translated from tree-sitter-dform/queries/indents.scm,
;;   matching `dform fmt' (docs/grammar.md "Formatting").
;; - Imenu for rules (by head predicate), modules, instances, resources
;;   (by type and name) and policies, and defun navigation
;;   (`C-M-a', `C-M-e', `C-M-h') over the same node types.
;; - An `eglot-server-programs' entry for `dform lsp' and two commands,
;;   `dform-select-environment' and `dform-why-at-point', wired to
;;   `eglot-execute-command'.  Start the server with `M-x eglot' in a
;;   .df buffer; it runs `dform lsp' from the project, and the commands
;;   send `dform.selectEnvironment' and `dform.why' to it.
;;
;; Installation (straight.el, this repository checked out locally):
;;
;;   (use-package dform-ts-mode
;;     :straight (:local-repo "/path/to/dform" :files ("editors/emacs/*.el")))
;;
;; Installation (package-vc, from a Git remote):
;;
;;   (use-package dform-ts-mode
;;     :vc (:url "https://example.com/simon/dform.git" :lisp-dir "editors/emacs"))
;;
;; Either way, `M-x treesit-install-language-grammar' (language `dform')
;; builds the grammar; see `treesit-language-source-alist' below for how
;; the recipe is found.

;;; Code:

(require 'treesit)

(declare-function treesit-parser-create "treesit.c")
(declare-function treesit-node-type "treesit.c")
(declare-function treesit-node-text "treesit.c")
(declare-function treesit-node-child-by-field-name "treesit.c")
(declare-function treesit-node-child "treesit.c")
(declare-function treesit-node-parent "treesit.c")
(declare-function treesit-induce-sparse-tree "treesit.c")
(declare-function treesit-search-subtree "treesit.c")

;; Eglot is built into Emacs 29+ but is not required here: these
;; commands are only usable once the user has `eglot' loaded (and a
;; `dform lsp' server running).  Declared, not required, so loading
;; `dform-ts-mode' does not force-load `eglot'.
(declare-function eglot-current-server "eglot")
(declare-function eglot-execute-command "eglot")
(declare-function eglot--TextDocumentIdentifier "eglot")
(declare-function eglot--pos-to-lsp-position "eglot")
(defvar eglot-server-programs)

;;; Grammar

;; The recipe below assumes this file is loaded from the dform
;; repository itself (editors/emacs/dform-ts-mode.el, sibling of
;; tree-sitter-dform/): a plain absolute path in the URL slot of
;; `treesit-language-source-alist' is enough, `treesit-install-language-
;; grammar' recognizes an existing directory and builds straight from
;; it (no Git clone, no `file://' scheme needed; see
;; `treesit--install-language-grammar-1' for the check).  Installed
;; standalone (straight's `:files' or package-vc's `:lisp-dir' bring
;; over only editors/emacs/*.el), that directory will not exist, so
;; replace this entry with the repository's own URL and
;; `:source-dir "tree-sitter-dform/src"' and run `M-x
;; treesit-install-language-grammar' for `dform'.
(defvar dform-ts-mode--grammar-dir
  (let ((here (or load-file-name buffer-file-name default-directory)))
    (expand-file-name "../../tree-sitter-dform" (file-name-directory here)))
  "Local checkout of tree-sitter-dform/, if this file is loaded from it.")

(add-to-list
 'treesit-language-source-alist
 (if (file-accessible-directory-p dform-ts-mode--grammar-dir)
     (list 'dform dform-ts-mode--grammar-dir)
   ;; Placeholder for a standalone install: replace with the real URL.
   '(dform "https://example.com/simon/dform.git" :source-dir "tree-sitter-dform/src")))

;;; Customization

(defgroup dform nil
  "Major mode for editing dform files."
  :group 'languages)

(defcustom dform-ts-mode-indent-offset 2
  "Number of columns for each indentation step in `dform-ts-mode'."
  :type 'integer
  :safe 'integerp
  :group 'dform)

(defface dform-reference-face
  '((t :inherit underline))
  "Face for a dform reference: a dot in a field-value position.

Proposal G (G-6): the value there is the attribute itself, an
apply-order edge, not its content read now (`docs/grammar.md'
\"Names\").  Bound to the tree-sitter grammar's
`@variable.reference' capture."
  :group 'dform)

;;; Syntax table

(defvar dform-ts-mode--syntax-table
  (let ((table (make-syntax-table)))
    (modify-syntax-entry ?_  "_"      table)
    (modify-syntax-entry ?\\ "\\"     table)
    (modify-syntax-entry ?+  "."      table)
    (modify-syntax-entry ?-  "."      table)
    (modify-syntax-entry ?=  "."      table)
    (modify-syntax-entry ?%  "."      table)
    (modify-syntax-entry ?<  "."      table)
    (modify-syntax-entry ?>  "."      table)
    (modify-syntax-entry ?|  "."      table)
    (modify-syntax-entry ?#  "<"      table)
    (modify-syntax-entry ?\n ">"      table)
    (modify-syntax-entry ?/  "."      table)
    table)
  "Syntax table for `dform-ts-mode'.")

;;; Font-lock

(defvar dform-ts-mode--font-lock-settings
  (treesit-font-lock-rules
   :language 'dform
   :feature 'comment
   ;; A doc comment (`#|' lines above an item) is a comment to the
   ;; grammar; its face says it is documentation.
   '(((comment) @font-lock-doc-face
      (:match "\\`#|" @font-lock-doc-face))
     (comment) @font-lock-comment-face)

   :language 'dform
   :feature 'string
   '((string) @font-lock-string-face)

   :language 'dform
   :feature 'escape-sequence
   :override t
   '((escape_sequence) @font-lock-escape-face
     (interpolation ["${" "}"] @font-lock-misc-punctuation-face))

   :language 'dform
   :feature 'number
   '((integer) @font-lock-number-face)

   :language 'dform
   :feature 'constant
   '([(true) (false)] @font-lock-constant-face
     (input name: (identifier) @font-lock-constant-face)
     (output name: (identifier) @font-lock-constant-face)
     (let name: (identifier) @font-lock-constant-face))

   :language 'dform
   :feature 'attribute
   '([(rank) (flag)] @font-lock-preprocessor-face)

   :language 'dform
   :feature 'type
   '((any_resource) @font-lock-type-face
     (dotted_name (identifier) @font-lock-type-face)
     (type name: (dotted_name (identifier) @font-lock-type-face)))

   :language 'dform
   :feature 'definition
   '((module name: (identifier) @font-lock-function-name-face)
     (policy name: (identifier) @font-lock-function-name-face)
     (use name: (identifier) @font-lock-function-name-face)
     (instance module: (identifier) @font-lock-function-name-face)
     (instance name: (identifier) @font-lock-function-name-face)
     (provider name: (identifier) @font-lock-function-name-face)
     (resource name: (identifier) @font-lock-function-name-face)
     (settings name: (identifier) @font-lock-function-name-face)
     (decl name: (dotted_name (identifier) @font-lock-function-name-face))
     (extern name: (dotted_name (identifier) @font-lock-function-name-face))
     (input_relation name: (identifier) @font-lock-function-name-face)
     (fact head: (call function: (identifier) @font-lock-function-name-face))
     (rule head: (call function: (identifier) @font-lock-function-name-face)))

   :language 'dform
   :feature 'function
   '((call function: (identifier) @font-lock-function-call-face)
     (call function: (member_expression field: (identifier) @font-lock-function-call-face)))

   :language 'dform
   :feature 'builtin
   ;; Emacs's query engine only supports `eq?', `match?' and
   ;; `pred?' predicates (no `#any-of?'), so a set of names is a regexp.
   "(call
      function: (identifier) @font-lock-builtin-face
      (#match? @font-lock-builtin-face
        \"^\\(count\\|sum\\|min\\|max\\|collect_set\\|collect_list\\|format\\|ref\\|attr\\|want\\|arg\\|setting\\|output\\|input\\|inet\\|inet_subnet\\|inet_host\\|declassify\\|cloud_attr\\|cloud_exists\\)$\"))
    ((identifier) @font-lock-builtin-face
      (#match? @font-lock-builtin-face \"^\\(settings\\|world\\)$\"))
    ((identifier) @font-lock-builtin-face
      (#eq? @font-lock-builtin-face \"_\"))"

   :language 'dform
   :feature 'property
   '((member_expression field: (identifier) @font-lock-property-use-face)
     (block_path (identifier) @font-lock-property-use-face)
     (object_field key: (identifier) @font-lock-property-name-face)
     (named_argument name: (identifier) @font-lock-property-name-face)
     (bind_arg name: (identifier) @font-lock-variable-name-face)
     (field_declaration name: (identifier) @font-lock-variable-name-face))

   :language 'dform
   :feature 'variable
   '((identifier) @font-lock-variable-name-face)

   ;; A field-value dot (docs/grammar.md "Names"; G-6). Whole-value
   ;; positions only: a field's value, a head/output argument, a list
   ;; or object element there, a comprehension item. Everywhere else a
   ;; dot reads. Overrides the plain `property'/`variable' faces above.
   :language 'dform
   :feature 'reference
   :override t
   '((field value: (member_expression) @dform-reference-face)
     (field value: (list (member_expression) @dform-reference-face))
     (field value: (object (object_field value: (member_expression) @dform-reference-face)))
     (output value: (member_expression) @dform-reference-face)
     (let value: (member_expression) @dform-reference-face)
     (set value: (member_expression) @dform-reference-face)
     (fact head: (call arguments: (arguments (member_expression) @dform-reference-face)))
     (rule head: (call arguments: (arguments (member_expression) @dform-reference-face)))
     (comprehension item: (member_expression) @dform-reference-face))

   :language 'dform
   :feature 'keyword
   '([
      "edition" "provider" "key" "import" "input" "from" "output" "export"
      "extern" "persist" "type" "decl" "mixed" "let" "set"
      "module" "instance" "policy" "use" "resource" "settings"
      ] @font-lock-keyword-face
     ["where" "check"] @font-lock-keyword-face
     ["deny" "warn"] @font-lock-keyword-face
     ["not" "in" "has"] @font-lock-keyword-face)

   :language 'dform
   :feature 'operator
   '(["=" "+=" "==" "!=" "<" "<=" ">" ">=" "+" "-" "*" "/" "%" "|"] @font-lock-operator-face)

   :language 'dform
   :feature 'bracket
   '(["(" ")" "[" "]" "{" "}"] @font-lock-bracket-face)

   :language 'dform
   :feature 'delimiter
   '(["," ":" "."] @font-lock-delimiter-face))
  "Font-lock settings for `dform-ts-mode'.")

(defvar dform-ts-mode--font-lock-feature-list
  '((comment definition)
    (keyword string type)
    (attribute builtin constant escape-sequence number property reference)
    (bracket delimiter function operator variable))
  "Font-lock feature list for `dform-ts-mode'.")

;;; Indentation

(defvar dform-ts-mode--indent-rules
  `((dform
     ;; A closer sits with the line that opened it.
     ((node-is ,(regexp-opt '("}" "]" ")"))) parent-bol 0)
     ;; A line is one step deeper than the innermost bracket, block or
     ;; body still open at its first token (docs/grammar.md
     ;; "Formatting").
     ((parent-is
       ,(regexp-opt
         '("block" "statement_block" "attribute_block" "body_block"
           "object" "record_type" "list" "comprehension"
           "arguments" "parenthesized" "index_expression")))
      parent-bol dform-ts-mode-indent-offset)
     ;; A block's clause (`where', after the block) or a one-line body.
     ((parent-is "clause") parent-bol dform-ts-mode-indent-offset)
     ((parent-is ,(regexp-opt '("rule" "check" "let" "set" "output")))
      parent-bol dform-ts-mode-indent-offset)
     (no-node parent-bol 0)
     (catch-all parent-bol 0)))
  "Indentation rules for `dform-ts-mode'.")

;;; Navigation and imenu

(defvar dform-ts-mode--defun-type-regexp
  (regexp-opt '("rule" "fact" "module" "instance" "resource" "policy"))
  "Regexp matching node types treated as defuns in `dform-ts-mode'.")

(defun dform-ts-mode--defun-name (node)
  "Return a name for NODE, a dform defun node, or nil.

Rules and facts are named by their head predicate; resources by
type and name; everything else (modules, instances, policies) by
its `name' field."
  (pcase (treesit-node-type node)
    ((or "rule" "fact")
     (when-let* ((head (treesit-node-child-by-field-name node "head")))
       (treesit-node-text
        (or (treesit-node-child-by-field-name head "function")
            (treesit-node-child-by-field-name head "name"))
        t)))
    ("resource"
     (when-let* ((type (treesit-node-child-by-field-name node "type"))
                 (name (treesit-node-child-by-field-name node "name")))
       (format "%s %s" (treesit-node-text type t) (treesit-node-text name t))))
    (_
     (when-let* ((name (treesit-node-child-by-field-name node "name")))
       (treesit-node-text name t)))))

(defvar dform-ts-mode--imenu-settings
  '(("Rule" "\\`\\(?:rule\\|fact\\)\\'" nil nil)
    ("Module" "\\`module\\'" nil nil)
    ("Instance" "\\`instance\\'" nil nil)
    ("Resource" "\\`resource\\'" nil nil)
    ("Policy" "\\`policy\\'" nil nil))
  "`treesit-simple-imenu-settings' for `dform-ts-mode'.")

;;; Eglot (the `dform lsp' language server)

(defun dform-ts-mode--eglot-server ()
  "Return the current buffer's eglot server, or signal a user error."
  (unless (fboundp 'eglot-current-server)
    (user-error "Eglot is not loaded"))
  (or (eglot-current-server)
      (user-error "No active eglot server for this buffer; start one with M-x eglot")))

(defun dform-select-environment (environment)
  "Ask the dform language server to select ENVIRONMENT, key values.

Sends the `dform.selectEnvironment' command via
`eglot-execute-command'.  Needs a running `dform lsp' server
\(start it with `M-x eglot')."
  (interactive "sEnvironment (k=v): ")
  (eglot-execute-command (dform-ts-mode--eglot-server)
                          "dform.selectEnvironment" (vector environment)))

(defun dform-why-at-point ()
  "Ask the dform language server why the value at point holds.

Sends the `dform.why' command via `eglot-execute-command' with the
current document and position (`eglot--TextDocumentIdentifier' and
`eglot--pos-to-lsp-position', both eglot internals: there is no
public equivalent for a caller-defined command's arguments).  Needs
a running `dform lsp' server (start it with `M-x eglot')."
  (interactive)
  (eglot-execute-command (dform-ts-mode--eglot-server)
                          "dform.why"
                          (vector `(:textDocument ,(eglot--TextDocumentIdentifier)
                                    :position ,(eglot--pos-to-lsp-position)))))

(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs '((dform-ts-mode) "dform" "lsp")))

;;; Major mode

;;;###autoload
(define-derived-mode dform-ts-mode prog-mode "dform"
  "Major mode for editing dform (.df) files, powered by tree-sitter."
  :group 'dform
  :syntax-table dform-ts-mode--syntax-table

  (unless (treesit-ready-p 'dform)
    (error "Tree-sitter for dform isn't available"))

  (setq treesit-primary-parser (treesit-parser-create 'dform))

  ;; Comments.
  (setq-local comment-start "# ")
  (setq-local comment-end "")
  (setq-local comment-start-skip "#+\\s-*")

  ;; Indentation.
  (setq-local indent-tabs-mode nil)
  (setq-local treesit-simple-indent-rules dform-ts-mode--indent-rules)

  ;; Navigation.
  (setq-local treesit-defun-type-regexp dform-ts-mode--defun-type-regexp)
  (setq-local treesit-defun-name-function #'dform-ts-mode--defun-name)

  ;; Font-lock.
  (setq-local treesit-font-lock-settings dform-ts-mode--font-lock-settings)
  (setq-local treesit-font-lock-feature-list dform-ts-mode--font-lock-feature-list)

  ;; Imenu.
  (setq-local treesit-simple-imenu-settings dform-ts-mode--imenu-settings)

  (treesit-major-mode-setup))

;;;###autoload
(add-to-list 'auto-mode-alist '("\\.df\\'" . dform-ts-mode))

(provide 'dform-ts-mode)

;;; dform-ts-mode.el ends here
