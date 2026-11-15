;;; code-trail.el --- Java navigation with code-trail -*- lexical-binding: t; -*-

;; Copyright (C) 2026

;; Author: code-trail contributors
;; Keywords: tools, languages
;; Version: 0.1.0
;; Package-Requires: ((emacs "27.1"))

;;; Commentary:

;; Provides Java definition navigation through the persistent `code-trail'
;; newline-delimited JSON process.  Navigation reads saved files only; save
;; modified Java buffers before requesting a definition.

;;; Code:

(require 'cl-lib)
(require 'json)
(require 'xref)
(require 'subr-x)

(defgroup code-trail nil
  "Java navigation with the code-trail executable."
  :group 'tools)

(defcustom code-trail-executable "code-trail"
  "Name or absolute path of the code-trail executable."
  :type 'string
  :group 'code-trail)

(defcustom code-trail-request-timeout 5
  "Maximum number of seconds to wait for a code-trail response."
  :type 'number
  :group 'code-trail)

(defvar code-trail--process nil
  "Shared persistent code-trail process.")
(defvar code-trail--next-id 0
  "Next JSON request identifier.")
(defvar code-trail--pending (make-hash-table :test #'eql)
  "Requests awaiting a response, keyed by correlation identifier.")

(cl-defstruct (code-trail--request
               (:constructor code-trail--make-request (&key process)))
  process done result error)

(defun code-trail--live-process ()
  "Return the shared live process, starting it when needed."
  (unless (and (processp code-trail--process)
               (process-live-p code-trail--process))
    (setq code-trail--process
          (make-process :name "code-trail"
                        :buffer nil
                        :command (list code-trail-executable)
                        :connection-type 'pipe
                        :coding 'utf-8-unix
                        :noquery t
                        :filter #'code-trail--process-filter
                        :sentinel #'code-trail--process-sentinel))
    (process-put code-trail--process 'code-trail-input ""))
  code-trail--process)

(defun code-trail--process-filter (process chunk)
  "Accumulate PROCESS output and dispatch complete newline-delimited replies."
  (let ((input (concat (process-get process 'code-trail-input) chunk))
        newline)
    (while (setq newline (string-match "\n" input))
      (let ((line (substring input 0 newline)))
        (setq input (substring input (1+ newline)))
        (unless (string-empty-p line)
          (condition-case err
              (code-trail--dispatch-response
               (json-parse-string line :object-type 'alist :array-type 'list
                                  :null-object nil :false-object nil))
            (error
             (message "code-trail: invalid response: %s"
                      (error-message-string err)))))))
    (process-put process 'code-trail-input input)))

(defun code-trail--dispatch-response (response)
  "Complete the pending request matching RESPONSE's correlation id."
  (let* ((id (alist-get 'id response))
         (request (and (integerp id) (gethash id code-trail--pending))))
    (when request
      (setf (code-trail--request-done request) t)
      (let ((error-object (alist-get 'error response)))
        (if error-object
            (setf (code-trail--request-error request)
                  (or (alist-get 'message error-object)
                      "Unknown code-trail error"))
          (setf (code-trail--request-result request)
                (alist-get 'result response)))))))

(defun code-trail--process-sentinel (process event)
  "Mark pending requests failed when PROCESS exits with EVENT."
  (unless (process-live-p process)
    (when (eq process code-trail--process)
      (setq code-trail--process nil))
    (maphash (lambda (_id request)
               (when (and (eq (code-trail--request-process request) process)
                          (not (code-trail--request-done request)))
                 (setf (code-trail--request-error request)
                       (format "code-trail process stopped: %s"
                               (string-trim event)))
                 (setf (code-trail--request-done request) t)))
             code-trail--pending)))

(defun code-trail--request (method params)
  "Send METHOD with PARAMS and synchronously return its response."
  (let* ((process (code-trail--live-process))
         (id (cl-incf code-trail--next-id))
         (request (code-trail--make-request :process process))
         (deadline (+ (float-time) code-trail-request-timeout))
         (payload (json-serialize
                   `((version . 1) (id . ,id) (method . ,method)
                     (params . ,params)))))
    (puthash id request code-trail--pending)
    (unwind-protect
        (progn
          (process-send-string process (concat payload "\n"))
          (while (and (not (code-trail--request-done request))
                      (process-live-p process)
                      (< (float-time) deadline))
            (accept-process-output process
                                   (max 0.001 (min 0.05 (- deadline (float-time))))))
          (unless (or (code-trail--request-done request)
                      (process-live-p process))
            (code-trail--process-sentinel process "process exited"))
          (unless (code-trail--request-done request)
            (user-error "Timed out waiting for code-trail after %s seconds"
                        code-trail-request-timeout))
          (when (code-trail--request-error request)
            (user-error "code-trail: %s" (code-trail--request-error request)))
          (code-trail--request-result request))
      (remhash id code-trail--pending))))

(defun code-trail--java-buffer-p ()
  "Return non-nil when current buffer uses a supported Java major mode."
  (or (derived-mode-p 'java-mode)
      (and (fboundp 'java-ts-mode) (derived-mode-p 'java-ts-mode))))

(defun code-trail--identifier-at-point ()
  "Return Java identifier at point, including Unicode characters and `$`."
  (when (code-trail--java-buffer-p)
    (let ((position (point)) start end)
      (save-excursion
        (beginning-of-line)
        (while (re-search-forward "[[:alnum:]_$]+" (line-end-position) t)
          (when (and (<= (match-beginning 0) position)
                     (<= position (match-end 0)))
            (setq start (match-beginning 0)
                  end (match-end 0)))))
      (when (and start (<= start position) (<= position end))
        (buffer-substring-no-properties start end)))))

(defun code-trail--backend ()
  "Return the code-trail xref backend when enabled in a Java buffer."
  (when (and code-trail-mode (code-trail--java-buffer-p))
    'code-trail))

(cl-defmethod xref-backend-identifier-at-point ((_backend (eql code-trail)))
  "Return Java identifier under point."
  (code-trail--identifier-at-point))

(cl-defmethod xref-backend-identifier-completion-table ((_backend (eql code-trail)))
  "Return completion table for the code-trail backend."
  (completion-table-dynamic (lambda (_prefix) nil)))

(defun code-trail--locations (method identifier)
  "Return METHOD xrefs for IDENTIFIER in the current saved Java buffer."
  (unless (code-trail--java-buffer-p)
    (user-error "code-trail supports Java buffers only"))
  (unless buffer-file-name
    (user-error "Save this Java buffer to a file before navigating"))
  (when (buffer-modified-p)
    (user-error "Save modified Java buffer first; code-trail reads saved files only"))
  (let* ((result (code-trail--request
                  method
                  `((file . ,(expand-file-name buffer-file-name))
                    (line . ,(line-number-at-pos))
                    (column . ,(- (point) (line-beginning-position)))
                    (symbol . ,identifier))))
         (locations result))
    (mapcar (lambda (location)
              (xref-make
               (alist-get 'summary location)
               (xref-make-file-location
                (alist-get 'file location)
                (alist-get 'line location)
                (alist-get 'column location))))
            locations)))

(cl-defmethod xref-backend-definitions ((_backend (eql code-trail)) identifier)
  "Return ranked definition xrefs for IDENTIFIER in the current Java buffer."
  (code-trail--locations "definition" identifier))

(cl-defmethod xref-backend-references ((_backend (eql code-trail)) identifier)
  "Return exact-name uses of IDENTIFIER, without semantic resolution."
  (code-trail--locations "references" identifier))

;;;###autoload
(defun code-trail-find-implementation (identifier)
  "Find implementations of IDENTIFIER using the standard xref result UI.
Interactively, use the Java identifier at point, or prompt with a prefix."
  (interactive
   (list (or (and (not current-prefix-arg)
                  (code-trail--identifier-at-point))
             (read-string "Find implementation of: "))))
  (unless (code-trail--backend)
    (user-error "Enable code-trail-mode in a Java buffer before navigating"))
  (xref-push-marker-stack)
  (xref-show-xrefs
   (apply-partially #'code-trail--locations "implementation" identifier)
   nil))

;;;###autoload
(define-minor-mode code-trail-mode
  "Enable code-trail xref navigation in Java buffers."
  :lighter " Trail"
  (if code-trail-mode
      (unless (code-trail--java-buffer-p)
        (setq code-trail-mode nil)
        (user-error "code-trail-mode is available only in java-mode and java-ts-mode"))
    (remove-hook 'xref-backend-functions #'code-trail--backend t))
  (when code-trail-mode
    (add-hook 'xref-backend-functions #'code-trail--backend nil t)))

;;;###autoload
(defun code-trail-stop ()
  "Stop the shared code-trail process and fail any outstanding requests."
  (interactive)
  (when (and (processp code-trail--process)
             (process-live-p code-trail--process))
    (delete-process code-trail--process))
  (setq code-trail--process nil))

(provide 'code-trail)

;;; code-trail.el ends here
