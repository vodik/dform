;;; dform-ts-mode-test.el --- Tests for dform-ts-mode -*- lexical-binding: t; -*-

;;; Commentary:

;; Run with:
;;
;;   emacs --batch -Q -L editors/emacs -l ert \
;;     -l editors/emacs/test/dform-ts-mode-test.el \
;;     -f ert-run-tests-batch-and-exit
;;
;; The tree-sitter-dform grammar is built once, into a temp directory
;; under this worktree (NOT /tmp: the system temp filesystem has a
;; tight per-user quota), and `treesit-extra-load-path' is pointed at
;; it, so the tests need only a C compiler and Git on PATH.

;;; Code:

(require 'ert)
(require 'treesit)
(require 'dform-ts-mode)

(defconst dform-ts-mode-test--dir
  (file-name-directory (or load-file-name buffer-file-name))
  "Directory holding this test file (editors/emacs/test/).")

(defconst dform-ts-mode-test--grammar-out-dir
  (expand-file-name "../../../.dform-ts-mode-test-grammar" dform-ts-mode-test--dir)
  "Build directory for the compiled grammar, under the worktree.")

(defun dform-ts-mode-test--ensure-grammar ()
  "Build tree-sitter-dform once and add it to `treesit-extra-load-path'."
  (unless (treesit-language-available-p 'dform)
    (make-directory dform-ts-mode-test--grammar-out-dir t)
    (treesit-install-language-grammar 'dform dform-ts-mode-test--grammar-out-dir)
    (add-to-list 'treesit-extra-load-path dform-ts-mode-test--grammar-out-dir)
    (unless (treesit-language-available-p 'dform)
      (ert-fail "Could not build tree-sitter-dform for the tests"))))

(defun dform-ts-mode-test--fixture (name)
  "Return the path of fixture NAME under test/fixtures/."
  (expand-file-name (concat "fixtures/" name) dform-ts-mode-test--dir))

(defun dform-ts-mode-test--face-at (string)
  "Return the face at the start of the first match of STRING in the buffer."
  (save-excursion
    (goto-char (point-min))
    (search-forward string)
    (goto-char (match-beginning 0))
    (get-text-property (point) 'face)))

(ert-deftest dform-ts-mode-test-reference-face ()
  "A reference in a field value gets `dform-reference-face'; a read
in a condition does not."
  (dform-ts-mode-test--ensure-grammar)
  (with-temp-buffer
    (insert-file-contents (dform-ts-mode-test--fixture "font-lock.df"))
    (dform-ts-mode)
    (font-lock-ensure)
    ;; `requester_vpc_id = a.id': a field value, so `a.id' is a
    ;; reference, not a read (proposal G, G-6).
    (should (eq (dform-ts-mode-test--face-at "a.id") 'dform-reference-face))
    ;; `not db.postgres["database::db"].multi_az': a check
    ;; condition, so this chain reads now and must not carry the
    ;; reference face.
    (should-not (eq (dform-ts-mode-test--face-at "db.postgres[")
                     'dform-reference-face))))

(ert-deftest dform-ts-mode-test-key-face ()
  "`key' starts a statement: a keyword."
  (dform-ts-mode-test--ensure-grammar)
  (with-temp-buffer
    (insert-file-contents (dform-ts-mode-test--fixture "font-lock.df"))
    (dform-ts-mode)
    (font-lock-ensure)
    (should (eq (dform-ts-mode-test--face-at "key env") 'font-lock-keyword-face))))

(ert-deftest dform-ts-mode-test-doc-comment-face ()
  "A `#|' doc comment gets `font-lock-doc-face'; a plain comment does not."
  (dform-ts-mode-test--ensure-grammar)
  (with-temp-buffer
    (insert "edition 2026\n# plain\n#| The input.\ninput x: int\n")
    (dform-ts-mode)
    (font-lock-ensure)
    (should (eq (dform-ts-mode-test--face-at "#| The") 'font-lock-doc-face))
    (should (eq (dform-ts-mode-test--face-at "# plain") 'font-lock-comment-face))))

(ert-deftest dform-ts-mode-test-range-face ()
  "A range's `..' and `..=' are operators (R-56)."
  (dform-ts-mode-test--ensure-grammar)
  (with-temp-buffer
    (insert "edition 2026\np(i) where n(k), i in 0..k, j in 1..=k\n")
    ;; Operators are a level 4 feature.
    (let ((treesit-font-lock-level 4))
      (dform-ts-mode))
    (font-lock-ensure)
    (should (eq (dform-ts-mode-test--face-at "..k") 'font-lock-operator-face))
    (should (eq (dform-ts-mode-test--face-at "..=") 'font-lock-operator-face))))

(ert-deftest dform-ts-mode-test-multiline-string ()
  "A string that spans lines (R-61) is a string on every line, and
indenting leaves its lines as written."
  (dform-ts-mode-test--ensure-grammar)
  (with-temp-buffer
    (insert "edition 2026\nresource t n {\n      a = \"x\n   y ${v}\n\"\n b = 1\n}\n")
    (dform-ts-mode)
    (font-lock-ensure)
    (should (eq (dform-ts-mode-test--face-at "   y") 'font-lock-string-face))
    (indent-region (point-min) (point-max))
    (should (equal (buffer-string)
                   "edition 2026\nresource t n {\n  a = \"x\n   y ${v}\n\"\n  b = 1\n}\n"))))

(ert-deftest dform-ts-mode-test-indent-round-trip ()
  "`indent-region' leaves a correctly indented file unchanged."
  (dform-ts-mode-test--ensure-grammar)
  (with-temp-buffer
    (insert-file-contents (dform-ts-mode-test--fixture "indent.df"))
    (dform-ts-mode)
    (let ((before (buffer-string)))
      (indent-region (point-min) (point-max))
      (should (equal (buffer-string) before)))))

(provide 'dform-ts-mode-test)

;;; dform-ts-mode-test.el ends here
