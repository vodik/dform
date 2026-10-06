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
    ;; `not db.postgres["database.db"].multi_az': a check
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
    (insert "# plain\n#| The input.\ninput x: int\n")
    (dform-ts-mode)
    (font-lock-ensure)
    (should (eq (dform-ts-mode-test--face-at "#| The") 'font-lock-doc-face))
    (should (eq (dform-ts-mode-test--face-at "# plain") 'font-lock-comment-face))))

(ert-deftest dform-ts-mode-test-range-face ()
  "A range's `..' and `..=' are operators (R-56)."
  (dform-ts-mode-test--ensure-grammar)
  (with-temp-buffer
    (insert "p(i) where n(k), i in 0..k, j in 1..=k\n")
    ;; Operators are a level 4 feature.
    (let ((treesit-font-lock-level 4))
      (dform-ts-mode))
    (font-lock-ensure)
    (should (eq (dform-ts-mode-test--face-at "..k") 'font-lock-operator-face))
    (should (eq (dform-ts-mode-test--face-at "..=") 'font-lock-operator-face))))

(ert-deftest dform-ts-mode-test-quantity-face ()
  "A quantity (`512Mi', `1h30m', `0.5') is a number (R-66)."
  (dform-ts-mode-test--ensure-grammar)
  (with-temp-buffer
    (insert "p(x) where x = 512Mi * 2, d = 1h30m, c = 0.5\n")
    (let ((treesit-font-lock-level 4))
      (dform-ts-mode))
    (font-lock-ensure)
    (should (eq (dform-ts-mode-test--face-at "512Mi") 'font-lock-number-face))
    (should (eq (dform-ts-mode-test--face-at "1h30m") 'font-lock-number-face))
    (should (eq (dform-ts-mode-test--face-at "0.5") 'font-lock-number-face))))

(ert-deftest dform-ts-mode-test-multiline-string ()
  "A string that spans lines (R-61) is a string on every line, and
indenting leaves its lines as written."
  (dform-ts-mode-test--ensure-grammar)
  (with-temp-buffer
    (insert "resource t n {\n      a = \"x\n   y ${v}\n\"\n b = 1\n}\n")
    (dform-ts-mode)
    (font-lock-ensure)
    (should (eq (dform-ts-mode-test--face-at "   y") 'font-lock-string-face))
    (indent-region (point-min) (point-max))
    (should (equal (buffer-string)
                   "resource t n {\n  a = \"x\n   y ${v}\n\"\n  b = 1\n}\n"))))

(ert-deftest dform-ts-mode-test-patterns ()
  "A tuple pattern (R-58) indents like a bracket, and the aggregate
`any' (R-59) is coloured as `count' is."
  (dform-ts-mode-test--ensure-grammar)
  (with-temp-buffer
    (insert "p(k, v) where labels(l), (\nk,\n v) in l\nq(b) where b = any(x), n = count(x), r(x)\n")
    (let ((treesit-font-lock-level 4))
      (dform-ts-mode))
    (font-lock-ensure)
    (should (eq (dform-ts-mode-test--face-at "any(")
                (dform-ts-mode-test--face-at "count(")))
    ;; An aggregate is a builtin, not a function call.
    (should (eq (dform-ts-mode-test--face-at "count(") 'font-lock-builtin-face))
    (indent-region (point-min) (point-max))
    (should (equal (buffer-string)
                   "p(k, v) where labels(l), (\n  k,\n  v) in l\nq(b) where b = any(x), n = count(x), r(x)\n"))))

(ert-deftest dform-ts-mode-test-indent-round-trip ()
  "`indent-region' leaves a correctly indented file unchanged."
  (dform-ts-mode-test--ensure-grammar)
  (with-temp-buffer
    (insert-file-contents (dform-ts-mode-test--fixture "indent.df"))
    (dform-ts-mode)
    (let ((before (buffer-string)))
      (indent-region (point-min) (point-max))
      (should (equal (buffer-string) before)))))

;;; Go-to-definition through `dform lsp' (R-78)

(defun dform-ts-mode-test--dform ()
  "The `dform' binary the xref tests run: $DFORM, else this worktree's
debug build; nil when neither is there."
  (let ((built (expand-file-name "../../../target/debug/dform" dform-ts-mode-test--dir)))
    (cond ((getenv "DFORM") (getenv "DFORM"))
          ((file-executable-p built) built))))

(defun dform-ts-mode-test--write (root files)
  "Write FILES, (NAME . TEXT) pairs, under ROOT."
  (dolist (f files)
    (let ((path (expand-file-name (car f) root)))
      (make-directory (file-name-directory path) t)
      (with-temp-file path (insert (cdr f))))))

(defun dform-ts-mode-test--definition (needle ahead)
  "The file and line of each definition xref finds for the name AHEAD
characters into the first NEEDLE in the current buffer."
  (goto-char (point-min))
  (search-forward needle)
  (goto-char (+ (match-beginning 0) ahead))
  (mapcar (lambda (item)
            (let ((loc (xref-item-location item)))
              (cons (xref-location-group loc) (xref-location-line loc))))
          (xref-backend-definitions
           'eglot (xref-backend-identifier-at-point 'eglot))))

(defun dform-ts-mode-test--with-server (files file body)
  "Write FILES, (NAME . TEXT) pairs, as a project; visit FILE in it under
eglot and `dform lsp', and call BODY there.  The caller skips the test
without a `dform' (`skip-unless' is ERT's, in a test's body)."
  (dform-ts-mode-test--ensure-grammar)
  (let ((dform (dform-ts-mode-test--dform)))
    (require 'eglot)
    (let* ((root (file-name-as-directory
                  (make-temp-file
                   (expand-file-name "xref-" dform-ts-mode-test--grammar-out-dir) t)))
           (process-environment
            (cons (concat "XDG_CACHE_HOME=" (expand-file-name "cache" root))
                  process-environment))
           (eglot-server-programs `((dform-ts-mode ,dform "lsp")))
           ;; The project is the directory (no VCS needed).
           (project-find-functions (list (lambda (_) (cons 'transient root))))
           (eglot-sync-connect t)
           (buffer nil))
      (unwind-protect
          (progn
            (dform-ts-mode-test--write root files)
            (setq buffer (find-file-noselect (expand-file-name file root)))
            (with-current-buffer buffer
              (dform-ts-mode)
              (eglot 'dform-ts-mode (cons 'transient root) 'eglot-lsp-server
                     (list dform "lsp") "dform")
              (should (eglot-managed-p))
              (funcall body root)))
        (when buffer
          (with-current-buffer buffer
            (when (eglot-current-server)
              (eglot-shutdown (eglot-current-server))))
          (kill-buffer buffer))
        (delete-directory root t)))))

(defconst dform-ts-mode-test--project
  '(("dform.toml" . "[project]\nname = \"xref\"\nedition = \"2026\"\n")
    ("config.df" . "let region: string = \"r1\"\n")
    ("stacks/s.df" . "use config\nprovider fake\nlet place = config.region\nlet net = inet.subnet(inet(\"10.0.0.0/8\"), 8, 1)\nresource compute.vm web {}\n"))
  "A project with a used module and a std function call.")

(ert-deftest dform-ts-mode-test-xref-definitions ()
  "`xref-find-definitions' through eglot and `dform lsp' finds a used
module's item (`config.region', in config.df) and a std function's
signature line (`inet.subnet', in the extracted std/inet.df)."
  (skip-unless (dform-ts-mode-test--dform))
  (dform-ts-mode-test--with-server
   dform-ts-mode-test--project "stacks/s.df"
   (lambda (root)
     (should (equal (dform-ts-mode-test--definition "config.region" 7)
                    (list (cons (expand-file-name "config.df" root) 1))))
     (let ((found (dform-ts-mode-test--definition "inet.subnet" 5)))
       (should (= (length found) 1))
       (should (string-suffix-p "std/inet.df" (caar found)))
       (with-temp-buffer
         (insert-file-contents (caar found))
         (forward-line (1- (cdar found)))
         (should (looking-at "fn subnet(")))))))

(ert-deftest dform-ts-mode-test-inlay-hints-setting ()
  "The server's inlay hints are off unless `dform-ts-mode-inlay-hints'."
  (skip-unless (dform-ts-mode-test--dform))
  (dform-ts-mode-test--with-server
   dform-ts-mode-test--project "stacks/s.df"
   (lambda (_root)
     (should-not (bound-and-true-p eglot-inlay-hints-mode))))
  (let ((dform-ts-mode-inlay-hints t))
    (dform-ts-mode-test--with-server
     dform-ts-mode-test--project "stacks/s.df"
     (lambda (_root)
       (should (bound-and-true-p eglot-inlay-hints-mode))))))

(provide 'dform-ts-mode-test)

;;; dform-ts-mode-test.el ends here
