local function email(value)
  return nodo.matches(value, "^[^@ ]+@[^@ ]+\\.[^@ ]+$"), "must be an email address"
end

return {
  api_version = 1,
  settings = {
    {
      name = "public_url",
      type = "url",
      label = "Public address",
      help = "The address the invite and personal links use, such as https://kringle.example.org. "
        .. "Empty, each link uses the address the browser used (--public-url).",
      validate = function(url)
        return nodo.matches(url, "^https?://[A-Za-z0-9.\\[\\]:-]+$"),
          "must be http:// or https:// with no path or trailing slash"
      end,
    },
    {
      name = "keep_days",
      type = "int",
      label = "Delete groups after (days)",
      help = "Groups are deleted this many days after they were started (--keep-days). Groups "
        .. "nobody but the host joined go after a week.",
      default = 120,
      min = 1,
      max = 3650,
    },

    -- Email. Each setting is named after the KRINGLE_* variable it fills (bin/kringle maps them).
    {
      name = "mail",
      type = "enum",
      label = "Email",
      help = "off: nothing is sent. tuta or smtp: invites, the draw and reminders are emailed. "
        .. "log: emails are written to the service's log instead, for testing.",
      choices = { "off", "tuta", "smtp", "log" },
      default = "off",
    },
    {
      name = "mail_from",
      type = "string",
      label = "Send from",
      help = "The sender address (default kringle@hollingworth.xyz). Tuta: the account's own "
        .. "address or one of its aliases.",
      validate = email,
    },
    {
      name = "mail_from_name",
      type = "string",
      label = "Sender name",
      help = "SMTP only (default Kringle); Tuta sends with the name set in Tuta.",
    },
    {
      name = "tuta_user",
      type = "string",
      label = "Tuta account",
      help = "The Tuta account's address. Accounts with two-factor sign-in cannot be used.",
      validate = email,
    },
    {
      name = "tuta_password",
      type = "secret",
      label = "Tuta password",
    },
    {
      name = "smtp_host",
      type = "string",
      label = "SMTP server",
      help = "Such as smtp.example.com.",
      validate = function(host)
        return nodo.matches(host, "^[A-Za-z0-9.-]+$"), "must be a host name"
      end,
    },
    {
      name = "smtp_port",
      type = "int",
      label = "SMTP port",
      help = "Empty: 465 for tls, 587 for starttls, 25 for none.",
      min = 1,
      max = 65535,
    },
    {
      name = "smtp_security",
      type = "enum",
      label = "SMTP security",
      choices = { "tls", "starttls", "none" },
      default = "tls",
    },
    {
      name = "smtp_user",
      type = "string",
      label = "SMTP user",
      help = "Empty if the server needs no login.",
    },
    {
      name = "smtp_password",
      type = "secret",
      label = "SMTP password",
    },
  },
}
