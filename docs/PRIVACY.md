# Privacy policy for the Tzibbur for Telegram bot

This policy covers the public bot instance. If you run your own instance, you are the operator.

**What the bot stores.** Your Telegram user id; your Tzibbur user id, phone number and display
name; the list of your Tzibbur groups and which Telegram topic belongs to each; message ids and
sequence numbers used to avoid duplicates; your Tzibbur session, encrypted, so the bot can stay
connected for you.

**What the bot does not store.** Message text is deleted from the server as soon as it has been
delivered to Telegram or confirmed by Tzibbur. SMS codes are never stored. Your Telegram messages
are not stored. Nothing about you is written to logs beyond ids and error codes.

**Who can see what.** The operator can see the ids and mappings above and, unavoidably, holds the
encrypted session tokens. The operator cannot read your Telegram chat history (Telegram does not
give bots that) and cannot read past messages from the bot, because there is no text on disk. A
malicious operator could modify the software to capture messages in transit; this is the same trust
you place in any hosted relay, and the code is open so it can be checked.

**Your controls.** `/disconnect` removes your session. "Disconnect and erase" removes every
mapping as well. `/privacy` in the bot shows a summary of this policy.

**Third parties.** Messages travel to and from Tzibbur (tzibbur.me) and Telegram under their own
policies. The bot uses no analytics or advertising services.

**Changes.** This document lives in the repository; its history is the change log.
