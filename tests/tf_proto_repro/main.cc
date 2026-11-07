#include "test.pb.h"
#include <iostream>

int main() {
  my_proto::MyMessage msg;
  msg.set_content("Rush Build");
  std::cout << "Content: " << msg.content() << std::endl;
  return 0;
}
