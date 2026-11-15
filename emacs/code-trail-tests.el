;;; code-trail-tests.el --- Tests for code-trail xref integration -*- lexical-binding: t; -*-

(require 'ert)
(require 'xref)
(require 'code-trail)

(defconst code-trail-test-executable
  (or (getenv "CODE_TRAIL_EXECUTABLE") "code-trail")
  "Real code-trail executable used by integration tests.")

(defun code-trail-test--write (directory name contents)
  "Write CONTENTS to NAME beneath DIRECTORY."
  (let ((file (expand-file-name name directory)))
    (with-temp-file file
      (insert contents))
    file))

(ert-deftest code-trail-xref-finds-java-definition ()
  "The public xref backend resolves saved Java source through the real binary."
  (let* ((code-trail-executable code-trail-test-executable)
         (root (make-temp-file "code-trail-project-" t))
         (definition (code-trail-test--write root "Target.java"
                                             "class Target {}\n"))
         (source (code-trail-test--write root "Caller.java"
                                         "class Caller { Target value; }\n"))
         implementation
         (buffer (generate-new-buffer " *code-trail-test*")))
    (unwind-protect
        (with-current-buffer buffer
          (java-mode)
          (insert-file-contents source)
          (setq buffer-file-name source)
          (set-buffer-modified-p nil)
          (goto-char (point-min))
          (search-forward "Target")
          (code-trail-mode 1)
          (let* ((backend (code-trail--backend))
                 (identifier (xref-backend-identifier-at-point backend))
                 (xrefs (xref-backend-definitions backend identifier))
                 (location (xref-item-location (car xrefs))))
            (should (equal identifier "Target"))
            (should xrefs)
            (should (equal (file-truename (xref-file-location-file location))
                           (file-truename definition)))
            (should (= (xref-file-location-line location) 1))
            (should (= (xref-file-location-column location) 6))
            (should-not (xref-backend-definitions backend "NotPresent"))
            (let* ((references (xref-backend-references backend identifier))
                   (use (xref-item-location (car references))))
              (should (= (length references) 1))
              (should (equal (file-truename (xref-file-location-file use))
                             (file-truename source)))
              (should (= (xref-file-location-line use) 1))
              (should (= (xref-file-location-column use) 15)))
            (should-not (xref-backend-references backend "NotPresent"))
            (setq implementation
                  (code-trail-test--write root "TargetImpl.java"
                                          "class TargetImpl extends Target {}\n"))
            (save-window-excursion
              (code-trail-find-implementation identifier)
              (with-current-buffer "*xref*"
                (should (string-match-p "class TargetImpl extends Target" (buffer-string)))))
            (let* ((implementations (code-trail--locations "implementation" identifier))
                   (target (xref-item-location (car implementations))))
              (should (equal (file-truename (xref-file-location-file target))
                             (file-truename implementation))))
            (set-buffer-modified-p t)
            (should-error
             (xref-backend-definitions backend "Target")
             :type 'user-error)
            (should-error
             (xref-backend-references backend "Target")
             :type 'user-error)
            (should-error
             (code-trail-find-implementation "Target")
             :type 'user-error)))
      (when (buffer-live-p buffer)
        (with-current-buffer buffer
          (code-trail-mode -1))
        (kill-buffer buffer))
      (delete-directory root t)
      (code-trail-stop))))

(ert-deftest code-trail-mode-disables-xref-backend ()
  "Disabling the minor mode removes its local xref backend."
  (with-temp-buffer
    (java-mode)
    (code-trail-mode 1)
    (should (memq #'code-trail--backend xref-backend-functions))
    (should (eq (funcall #'code-trail--backend) 'code-trail))
    (code-trail-mode -1)
    (should-not (memq #'code-trail--backend xref-backend-functions))
    (should-not code-trail-mode)))

(provide 'code-trail-tests)

;;; code-trail-tests.el ends here
