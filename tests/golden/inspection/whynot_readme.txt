net.subnet["private-us-east-1c"]: no rule derives it
  stacks/shop.df:13  resource net.subnet "private-${availability_zone}" { .. } where availability_zone("available", availability_zone, n)
    availability_zone("available", "us-east-1c", n): no row
    nearest: ("us-east-1a", 0), ("us-east-1b", 1)
