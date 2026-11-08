#!/bin/awk

BEGIN {
  OFS = " | "
  FS = " *= *"
  if (registry_tsv_file != "") {
    while ((getline reg_line < registry_tsv_file) > 0) {
      n_fields = split(reg_line, f, "\t")
      if (n_fields < 5 || f[1] == "") continue
      r_tool = f[1]
      r_cat = f[2]
      r_contrib = (f[3] == "true")
      r_deprecated = (f[4] == "true")
      r_summary = f[5]
      registered_tools[r_tool] = 1

      if (!show_deprecated && r_deprecated) continue
      if (hide_contrib && r_contrib) continue
      if (r_contrib) r_summary = "(contrib) " r_summary
      if (r_deprecated) r_summary = "(DEPRECATED) " r_summary
      if (r_cat == "") {
        r_cat = "unknown category"
      } else {
        r_cat = toupper(substr(r_cat, 1, 1)) tolower(substr(r_cat, 2))
      }
      reg_by_cat[r_cat] = reg_by_cat[r_cat] "  " r_tool " | " r_summary "\n"
    }
    close(registry_tsv_file)
  }
}

{
  if (FILENAME != filename) {
    process()
    filename = FILENAME
    summary = ""
    category = ""
    deprecated = 0
    contrib = 0
  }
}

/^### / {
  if (summary == "") {
    summary = substr($0, 5)
    next
  }
}

/^#### +CATEGORY *=/ {
  if (category == "") {
    category = toupper(substr($2,1,1)) tolower(substr($2,2))
    next
  }
}

/^#### +DEPRECATED/ {
  deprecated = 1
}

function process() {
  if (filename && (show_deprecated || !deprecated)) {
    toolname = filename
    sub(/.fx$/, "", toolname)
    sub(/^.*\//, "",  toolname)

    if (match(filename, "/contrib/")) {
      summary = "(contrib) " summary
      contrib = 1
    }

    if (!contrib || !hide_contrib) {
      if (match(filename, "^.*/vendor/")) {
        vendor_starts=RLENGTH+1
        if (match(filename, "[^/]+/scripts/devshell/")) {
          toolname = "vendor " substr(filename, vendor_starts, RLENGTH - length("/scripts/devshell/")) " " toolname
        }
      }

      if (!category) {
        category = "unknown category"
      }

      if (toolname in registered_tools) {
        return
      }

      if (deprecated == 1) {
        summary = "(DEPRECATED) " summary
      }
      by_cat[category] = by_cat[category] "  " toolname " | " summary "\n"
    }
  }
}

function print_category(category) {
  if (by_cat[category]) {
    print category ":"
    print by_cat[category] | command
    close(command)
    print ""
    delete by_cat[category]
  }
}

END {
  process()
  for (cat in reg_by_cat) {
    by_cat[cat] = by_cat[cat] reg_by_cat[cat]
  }
  command="sort | column -t -s '|' -c 2"

  # print known categories in a specific order
  print_category("Source tree")
  print_category("Documentation")
  print_category("Build")
  print_category("Device discovery")
  print_category("Device management")
  print_category("Software delivery")
  print_category("Run, inspect and debug")
  print_category("Other")

  # print the remaining categories in no defined order (no easy way to reorder
  # an array in posix awk)
  for (category in by_cat) {
    print_category(category)
  }
}

