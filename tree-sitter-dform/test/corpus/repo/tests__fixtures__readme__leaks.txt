==============================
tests/fixtures/readme/leaks.df
==============================

output password: string = pw # E0304: not declared secret(string)

use aws { region = "us-east-1" }

let pw = random.password("db")

deny "short password" where pw.len < 12 # E0301: inspecting it leaks it
resource aws.iam_user "u-${pw}" { name = "x" } # E0305: addresses are printed

---

(source_file
  (output
    (identifier)
    (type
      (dotted_name
        (identifier)))
    (identifier))
  (comment)
  (use
    (dotted_name
      (identifier))
    (block
      (field
        (block_path
          (identifier))
        (string
          (string_content)))))
  (let
    (identifier)
    (call
      (member_expression
        (identifier)
        (identifier))
      (arguments
        (string
          (string_content)))))
  (check
    (string
      (string_content))
    (body
      (comparison
        (member_expression
          (identifier)
          (identifier))
        (integer))))
  (comment)
  (resource
    (dotted_name
      (identifier)
      (identifier))
    (string
      (string_content)
      (interpolation
        (identifier)))
    (block
      (field
        (block_path
          (identifier))
        (string
          (string_content)))))
  (comment))
